use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use wasmtime::*;

use crate::shm::{expand_mapping, map_into_memory};

use common::{RegistryEntry, Superblock, INITIAL_SHM_SIZE, REGISTRY_OFFSET, REGISTRY_SIZE, TARGET_OFFSET, WASM_PATH};

use crate::runtime::organizer::BucketOrganizer;
use crate::policy::{LastWriteWinsPolicy, MajorityWinsPolicy, MaxIdWinsPolicy, MinIdWinsPolicy};
use crate::routing::stream::HostStream;
use crate::routing::aggregate::AggregateConnection;
use crate::routing::broadcast::BroadcastConnection;
use crate::routing::shuffle::ShuffleConnection;
use crate::policy::{ModuloPartition, RoundRobinPartition, FixedMapPartition};

// Config constants
const WRITER_TOTAL_ITEMS: u32 = 5000;
const READER_TOTAL_POLLS: u32 = 15;
const READER_POLL_INTERVAL_MS: u64 = 200;

pub struct WorkerState {
    pub file: File,
    pub splice_addr: usize,
}

/// Creates a Wasmtime engine configured for shared memory:
/// 4GB static address space, no guard pages (VMA is managed manually), threads enabled.
pub fn create_wasmtime_engine() -> Result<Engine> {
    let mut config = Config::new();
    // Allow full 4GB address space
    config.static_memory_maximum_size(1 << 32);
    // Disable guard pages as we manage VMA manually
    config.static_memory_guard_size(0);
    config.dynamic_memory_guard_size(0);
    // Enable threads for Shared Memory
    config.wasm_threads(true);

    Engine::new(&config)
}

/// Allocates the WASM shared memory, maps the SHM file into it at `TARGET_OFFSET`,
/// and registers the two host imports (`host_remap`, `host_resolve_atomic`) with the linker.
/// Returns the `Memory` handle needed for direct host-side reads after WASM execution.
pub fn setup_vma_environment(
    store: &mut Store<WorkerState>,
    linker: &mut Linker<WorkerState>,
    file: &File,
) -> Result<Memory> {
    let memory_ty = MemoryType::shared(49152, 65536);
    let memory = Memory::new(&mut *store, memory_ty)?;

    let base_ptr = memory.data_ptr(&*store);
    let splice_addr = unsafe { base_ptr.add(TARGET_OFFSET) } as usize;
    store.data_mut().splice_addr = splice_addr;

    let current_file_size = file.metadata()?.len() as usize;
    let required_min_size = common::INITIAL_SHM_SIZE as usize;
    let map_size = std::cmp::max(current_file_size, required_min_size);
    if current_file_size < map_size {
        file.set_len(map_size as u64)?;
    }
    
    map_into_memory(file, splice_addr, map_size)?;

    linker.func_wrap(
        "env",
        "host_remap",
        |mut caller: Caller<'_, WorkerState>, new_size: u32| {
            let state = caller.data_mut();
            println!(
                "[PID:{}] remap to {} MB",
                std::process::id(),
                new_size / 1024 / 1024
            );
            expand_mapping(&state.file, state.splice_addr, new_size as usize).unwrap();
        },
    )?;

    let memory_handle = memory; // Capture memory handle

    linker.func_wrap(
        "env",
        "host_resolve_atomic",
        move |mut caller: Caller<'_, WorkerState>, ptr: u32, len: u32| -> u32 {
            let base_ptr = memory_handle.data_ptr(&caller);
            let name_bytes =
                unsafe { std::slice::from_raw_parts(base_ptr.add(ptr as usize), len as usize) };

            let name_len = std::cmp::min(name_bytes.len(), 52); // change to 52
            let mut entry_name = [0u8; 52];
            entry_name[..name_len].copy_from_slice(&name_bytes[..name_len]);

            let host_base = unsafe { memory_handle.data_ptr(&caller).add(TARGET_OFFSET as usize) };
            let superblock = unsafe { &*(host_base as *const Superblock) };
            let lock = &superblock.registry_lock;
            let registry_base = unsafe { host_base.add(REGISTRY_OFFSET as usize) };

            // --- Spinlock Acquire ---
            while lock
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                std::hint::spin_loop();
            }

            // --- Search Registry ---
            let mut result_index = u32::MAX;
            let next_idx_atomic = &superblock.next_atomic_idx;
            let current_count = next_idx_atomic.load(Ordering::Relaxed);

            for i in 0..current_count {
                let entry_ptr = unsafe {
                    registry_base.add(i as usize * std::mem::size_of::<RegistryEntry>()) as *const RegistryEntry
                };
                let entry = unsafe { &*entry_ptr };
                if entry.name == entry_name {
                    result_index = entry.index;
                    break;
                }
            }

            // --- Register New Name ---
            if result_index == u32::MAX {
                result_index = current_count;

                let registry_entry_base = registry_base as *mut RegistryEntry;
                let new_entry_ptr = unsafe { registry_entry_base.add(current_count as usize) };

                unsafe {
                    core::ptr::write(
                        new_entry_ptr,
                        RegistryEntry {
                            name: entry_name,
                            index: result_index,
                            payload_offset: core::sync::atomic::AtomicU32::new(0),
                            payload_len: core::sync::atomic::AtomicU32::new(0),
                        },
                    );
                }

                next_idx_atomic.store(current_count + 1, Ordering::Relaxed);
            }

            // --- Spinlock Release ---
            lock.store(0, Ordering::Release);

            result_index
        },
    )?;

    linker.define(&mut *store, "env", "memory", memory)?;
    Ok(memory)
}

/// Loads the guest WASM module and executes the function for `role` with the given `id`.
/// Supported roles: `"writer"`, `"reader"`, `"func_a"`, `"func_b"`.
/// After writers complete, triggers the `BucketOrganizer` to resolve conflicts and GC pages.
pub fn run_worker(role: &str, shm_path: &str, id: u32) -> Result<()> {
    let pid = std::process::id();
    let file = OpenOptions::new().read(true).write(true).open(shm_path)?;

    // 1. Create Engine (New Function)
    let engine = create_wasmtime_engine()?;

    let mut store = Store::new(
        &engine,
        WorkerState {
            file: file.try_clone()?,
            splice_addr: 0,
        },
    );

    let mut linker = Linker::new(&engine);

    // 2. Setup VMA & Exports (New Function)
    // We capture the memory instance here to use it for direct host reading later
    let memory = setup_vma_environment(&mut store, &mut linker, &file)?;

    let module = Module::from_file(&engine, WASM_PATH)?;
    let instance = linker.instantiate(&mut store, &module)?;

    if role == "writer" {
        let func = instance.get_typed_func::<u32, ()>(&mut store, "writer")?;
        println!("[Writer ID:{}] (PID:{}) Started...", id, pid);
        for i in 1..=WRITER_TOTAL_ITEMS {
            func.call(&mut store, id)?;
        }
        println!("[Writer ID:{}] Finished!", id);
    } else if role == "reader" {
        let read_snapshot = instance.get_typed_func::<u32, u64>(&mut store, "reader")?;

        for _ in 0..READER_TOTAL_POLLS {
            let packed_ptr_len = read_snapshot.call(&mut store, id)?;

            // Direct Host Read (Bypassing Wasm)
            // Note: We skip the Registry area to read the first atomic variable (Index 0)
            let base_ptr = memory.data_ptr(&store);
            let global_atomic_val = unsafe {
                let offset = TARGET_OFFSET + (REGISTRY_OFFSET as usize) + (REGISTRY_SIZE as usize);
                let ptr = base_ptr.add(offset) as *const std::sync::atomic::AtomicU64;
                (*ptr).load(std::sync::atomic::Ordering::SeqCst)
            };

            if packed_ptr_len > 0 {
                let ptr = (packed_ptr_len >> 32) as usize;
                let len = (packed_ptr_len & 0xFFFFFFFF) as usize;

                let raw_bytes = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                let data_str = String::from_utf8_lossy(raw_bytes);

                println!(
                    "[Reader ID:{}] JSON: {} | Global_Atomic(Index 0): {}",
                    id, data_str, global_atomic_val
                );
            } else {
                println!(
                    "[Reader ID:{}] Waiting... Global_Atomic(Index 0): {}",
                    id, global_atomic_val
                );
            }
            thread::sleep(Duration::from_millis(READER_POLL_INTERVAL_MS));
        }
    }

    if role == "writer" {
        let organizer = BucketOrganizer::new(&mut store, &memory);

        unsafe {
            organizer.consume_all_buckets(MajorityWinsPolicy);
        }

        println!("[System] Organization & Garbage Collection Completed.");
    }

    match role {
        "func_a" => {
            let func = instance.get_typed_func::<u32, ()>(&mut store, "func_a")?;
            println!("[Host] Running Func A (Writer)...");

            
            for i in 1..=15 {
                func.call(&mut store, i)?;
            }
            println!("[Host] Func A execution completed.");
        }
        "func_b" => {
            let func = instance.get_typed_func::<u32, ()>(&mut store, "func_b")?;
            println!("[Host] Running Func B (Reader)...");

            func.call(&mut store, id)?;
            println!("[Host] Func B execution completed.");

            
            let base_ptr = memory.data_ptr(&store);
            let target_base = unsafe { base_ptr.add(TARGET_OFFSET) };

            
            let log_offset = unsafe { &*(target_base as *const Superblock) }
                .log_offset.load(Ordering::Acquire);

            if log_offset > 0 {
                let log_data_ptr = unsafe { target_base.add(common::LOG_ARENA_OFFSET as usize) };
                let log_bytes =
                    unsafe { std::slice::from_raw_parts(log_data_ptr, log_offset as usize) };
                let log_str = String::from_utf8_lossy(log_bytes);
                println!("\n========= GUEST LOG =========");
                print!("{}", log_str);
                println!("=============================\n");
            }
        }
        // ─────────────────────────────────────────────────────────────────
        // Routing test: HostStream 1→1 bridge
        //
        // Producer 0 writes 3 records to stream slot 0.
        // Host calls HostStream::bridge(0 → 1): two atomic stores redirect
        // slot 1's head/tail to slot 0's existing page chain (zero copy).
        // WASM consumer reads from slot 1 — the last record must come from
        // producer 0, proving the bridge wired the chains correctly.
        // ─────────────────────────────────────────────────────────────────
        "stream_bridge_test" => {
            println!("\n[StreamBridgeTest] === HostStream 1→1 bridge ===");
            let produce  = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream")?;
            let consume  = instance.get_typed_func::<u32, u64>(&mut store, "consume_routed_stream")?;

            // Step 1: producer 0 populates slot 0
            produce.call(&mut store, 0)?;
            println!("[StreamBridgeTest] Producer 0 wrote 3 records to slot 0.");

            // Step 2: host bridges slot 0 → slot 1 (zero-copy, two atomic stores)
            let splice_addr = store.data().splice_addr;
            let bridged = HostStream::new(splice_addr).bridge(0, 1);
            println!("[StreamBridgeTest] HostStream::bridge(0 → 1): {}",
                if bridged { "OK" } else { "FAIL (slot 0 was empty)" });

            // Step 3: WASM consumer reads from slot 1
            let packed = consume.call(&mut store, 1)?;
            let base_ptr = memory.data_ptr(&store);
            if packed > 0 {
                let ptr = (packed >> 32) as usize;
                let len = (packed & 0xFFFF_FFFF) as usize;
                let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                println!("[StreamBridgeTest] Slot 1 last record: \"{}\"  ✓",
                    String::from_utf8_lossy(raw));
            } else {
                println!("[StreamBridgeTest] Slot 1 is empty after bridge — FAIL");
            }
            println!("[StreamBridgeTest] Done.\n");
        }

        // ─────────────────────────────────────────────────────────────────
        // Routing test: AggregateConnection N→1 merge
        //
        // Producers 0, 1, 2 each write 3 records to their own stream slots.
        // Host calls AggregateConnection::bridge([0,1,2] → 3): each source
        // chain is spliced onto slot 3's tail via O(1) chain_onto calls
        // (sequential here — N=3 ≤ PARALLEL_THRESHOLD=50).
        // WASM consumer reads from slot 3 — the last record must come from
        // producer 2 (the last chain merged), confirming all three chains
        // are now reachable from slot 3's head.
        // ─────────────────────────────────────────────────────────────────
        "aggregate_test" => {
            println!("\n[AggregateTest] === AggregateConnection N→1 merge ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            // Step 1: three producers populate slots 0, 1, 2
            for i in 0..3u32 {
                produce.call(&mut store, i)?;
                println!("[AggregateTest] Producer {} wrote 3 records to slot {}.", i, i);
            }

            // Step 2: host merges chains [0,1,2] into slot 3
            let splice_addr = store.data().splice_addr;
            AggregateConnection::new(&[0, 1, 2], 3).bridge(splice_addr);
            println!("[AggregateTest] AggregateConnection::bridge([0,1,2] → 3) done.");

            // Step 3: dump all records from slot 3 — must see 9 records total
            // (3 from each producer, in chain order: P0 → P1 → P2)
            let packed = dump.call(&mut store, 3)?;
            let base_ptr = memory.data_ptr(&store);
            if packed > 0 {
                let ptr = (packed >> 32) as usize;
                let len = (packed & 0xFFFF_FFFF) as usize;
                let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                println!("[AggregateTest] Slot 3 all records:");
                for (i, line) in String::from_utf8_lossy(raw).lines().enumerate() {
                    println!("  [{:2}] {}", i, line);
                }
            } else {
                println!("[AggregateTest] Slot 3 is empty after aggregate — FAIL");
            }
            println!("[AggregateTest] Done.\n");
        }

        // ─────────────────────────────────────────────────────────────────
        // Routing test (light): ShuffleConnection — ModuloPartition, 2→2
        //
        // 3 records per producer, all printed in full.
        // upstream 0 % 2 = 0 → slot 2  |  upstream 1 % 2 = 1 → slot 3
        // ─────────────────────────────────────────────────────────────────
        "shuffle_test" => {
            println!("\n[ShuffleTest] === ModuloPartition 2→2 (light: 3 records/upstream) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            for i in 0..2u32 {
                produce.call(&mut store, i)?;
                println!("[ShuffleTest] Producer {} wrote 3 records to slot {}.", i, i);
            }

            let splice_addr = store.data().splice_addr;
            ShuffleConnection::new(&[0, 1], &[2, 3], ModuloPartition).bridge(splice_addr);
            println!("[ShuffleTest] bridge([0,1] → [2,3], ModuloPartition) done.");

            let base_ptr = memory.data_ptr(&store);
            for slot in [2u32, 3u32] {
                let packed = dump.call(&mut store, slot)?;
                if packed > 0 {
                    let ptr = (packed >> 32) as usize;
                    let len = (packed & 0xFFFF_FFFF) as usize;
                    let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                    println!("[ShuffleTest] Slot {} all records:", slot);
                    for (i, line) in String::from_utf8_lossy(raw).lines().enumerate() {
                        println!("  [{:2}] {}", i, line);
                    }
                } else {
                    println!("[ShuffleTest] Slot {} is empty — FAIL", slot);
                }
            }
            println!("[ShuffleTest] Done.\n");
        }

        // ─────────────────────────────────────────────────────────────────
        // Routing test (light): ShuffleConnection — RoundRobinPartition, 2→2
        //
        // 3 records per producer, all printed in full.
        // Upstreams reversed [1,0]: RoundRobin call-order assigns
        //   1st (up=1) → slot 2,  2nd (up=0) → slot 3
        // — the opposite of what ModuloPartition produces.
        // ─────────────────────────────────────────────────────────────────
        "shuffle_roundrobin_test" => {
            println!("\n[ShuffleRRTest] === RoundRobinPartition 2→2 (light: 3 records/upstream) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            for i in 0..2u32 {
                produce.call(&mut store, i)?;
                println!("[ShuffleRRTest] Producer {} wrote 3 records to slot {}.", i, i);
            }

            let splice_addr = store.data().splice_addr;
            ShuffleConnection::new(&[1, 0], &[2, 3], RoundRobinPartition::new()).bridge(splice_addr);
            println!("[ShuffleRRTest] bridge([1,0] → [2,3], RoundRobin) done.");
            println!("[ShuffleRRTest] Expected: slot 2 = P1 records, slot 3 = P0 records.");

            let base_ptr = memory.data_ptr(&store);
            for slot in [2u32, 3u32] {
                let packed = dump.call(&mut store, slot)?;
                if packed > 0 {
                    let ptr = (packed >> 32) as usize;
                    let len = (packed & 0xFFFF_FFFF) as usize;
                    let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                    println!("[ShuffleRRTest] Slot {} all records:", slot);
                    for (i, line) in String::from_utf8_lossy(raw).lines().enumerate() {
                        println!("  [{:2}] {}", i, line);
                    }
                } else {
                    println!("[ShuffleRRTest] Slot {} is empty — FAIL", slot);
                }
            }
            println!("[ShuffleRRTest] Done.\n");
        }

        // ─────────────────────────────────────────────────────────────────
        // Routing test (light): ShuffleConnection — FixedMapPartition, 2→2 swapped
        //
        // 3 records per producer, all printed in full.
        // map = {0→1, 1→0}: upstream 0 → slot 3, upstream 1 → slot 2
        // ─────────────────────────────────────────────────────────────────
        "shuffle_fixedmap_test" => {
            println!("\n[ShuffleFMTest] === FixedMapPartition 2→2 swapped (light: 3 records/upstream) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            for i in 0..2u32 {
                produce.call(&mut store, i)?;
                println!("[ShuffleFMTest] Producer {} wrote 3 records to slot {}.", i, i);
            }

            let splice_addr = store.data().splice_addr;
            ShuffleConnection::new(
                &[0, 1], &[2, 3],
                FixedMapPartition::new([(0, 1), (1, 0)].into_iter().collect(), 0),
            ).bridge(splice_addr);
            println!("[ShuffleFMTest] bridge([0,1] → [2,3], FixedMap{{0→1,1→0}}) done.");
            println!("[ShuffleFMTest] Expected: slot 2 = P1 records, slot 3 = P0 records.");

            let base_ptr = memory.data_ptr(&store);
            for slot in [2u32, 3u32] {
                let packed = dump.call(&mut store, slot)?;
                if packed > 0 {
                    let ptr = (packed >> 32) as usize;
                    let len = (packed & 0xFFFF_FFFF) as usize;
                    let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                    println!("[ShuffleFMTest] Slot {} all records:", slot);
                    for (i, line) in String::from_utf8_lossy(raw).lines().enumerate() {
                        println!("  [{:2}] {}", i, line);
                    }
                } else {
                    println!("[ShuffleFMTest] Slot {} is empty — FAIL", slot);
                }
            }
            println!("[ShuffleFMTest] Done.\n");
        }

        // ─────────────────────────────────────────────────────────────────
        // Routing test (heavy): ShuffleConnection — ModuloPartition, 50→10
        //
        // 50 producers write 150 records each (~70 bytes/record) to slots
        // 0-49, producing ~2–3 page chains per upstream slot and ~525 KB of
        // stream data in total.
        //
        // ModuloPartition (upstream_id % 10) fans in 5 producers per slot:
        //   slot 50: producers 0,10,20,30,40  (750 records)
        //   slot 51: producers 1,11,21,31,41  (750 records)
        //   ...
        //   slot 59: producers 9,19,29,39,49  (750 records)
        //
        // The 10 downstream groups are merged concurrently (thread::scope);
        // each group's 5-chain sequential merge is well under PARALLEL_THRESHOLD.
        // ─────────────────────────────────────────────────────────────────
        "shuffle_heavy_test" => {
            println!("\n[ShuffleHeavyTest] === ModuloPartition 50→10 (150 records × 50 producers) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream_heavy")?;
            let count   = instance.get_typed_func::<u32, u32>(&mut store, "count_stream_records")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            // Step 1: 50 producers write to slots 0-49
            for i in 0..50u32 { produce.call(&mut store, i)?; }
            println!("[ShuffleHeavyTest] 50 producers wrote 150 records each to slots 0-49  \
                      (~{} KB total).", 50 * 150 * 70 / 1024);

            // Step 2: route — up % 10 assigns each producer to one of 10 downstream slots
            let upstreams:   Vec<usize> = (0..50).collect();
            let downstreams: Vec<usize> = (50..60).collect();
            let splice_addr = store.data().splice_addr;
            ShuffleConnection::new(&upstreams, &downstreams, ModuloPartition).bridge(splice_addr);
            println!("[ShuffleHeavyTest] bridge([0..49] → [50..59], ModuloPartition(% 10)) done.");

            // Step 3: verify all 10 downstream slots — each must hold exactly 750 records
            println!("[ShuffleHeavyTest] Downstream summary (expect 750 records each):");
            let base_ptr = memory.data_ptr(&store);
            let mut all_ok = true;
            for slot in 50u32..60 {
                let n = count.call(&mut store, slot)?;
                let ok = n == 750;
                all_ok &= ok;
                println!("  slot {:2}: {:4} records  {}", slot, n, if ok { "✓" } else { "FAIL" });
            }

            // Step 4: spot-check slot 50 (first producers 0,10,20,30,40) and
            //         slot 59 (last producers 9,19,29,39,49)
            for spot in [50u32, 59u32] {
                let packed = dump.call(&mut store, spot)?;
                if packed > 0 {
                    let ptr = (packed >> 32) as usize;
                    let len = (packed & 0xFFFF_FFFF) as usize;
                    let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                    let s   = String::from_utf8_lossy(raw);
                    let lines: Vec<&str> = s.lines().collect();
                    let total = lines.len();
                    println!("[ShuffleHeavyTest] Spot-check slot {}:", spot);
                    for (i, l) in lines.iter().take(3).enumerate() { println!("  [{:3}] {}", i, l); }
                    if total > 6 { println!("  ... {} records omitted ...", total - 6); }
                    for i in total.saturating_sub(3)..total { println!("  [{:3}] {}", i, lines[i]); }
                }
            }
            println!("[ShuffleHeavyTest] Overall: {}", if all_ok { "✓ ALL PASS" } else { "FAIL" });
            println!("[ShuffleHeavyTest] Done.\n");
        }

        // ─────────────────────────────────────────────────────────────────
        // Routing test (heavy): ShuffleConnection — RoundRobinPartition, 50→10
        //
        // Same 50 producers and 10 downstream slots as the Modulo test, but
        // the upstream list is supplied in REVERSE ORDER [49..0].  RoundRobin
        // assigns by call order (not ID value), so the producers landing in
        // each slot differ entirely from the Modulo result:
        //
        //   Modulo slot 50 ← producers {0,10,20,30,40}
        //   RR     slot 50 ← producers {49,39,29,19,9}  (call positions 0,10,20,30,40)
        //
        // Record count per slot is the same (750), but the chain heads carry
        // different producer IDs — visible in the first-record spot-checks.
        // ─────────────────────────────────────────────────────────────────
        "shuffle_roundrobin_heavy_test" => {
            println!("\n[ShuffleRRHeavyTest] === RoundRobinPartition 50→10 reversed (150 records × 50 producers) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream_heavy")?;
            let count   = instance.get_typed_func::<u32, u32>(&mut store, "count_stream_records")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            // Step 1: 50 producers write to slots 0-49
            for i in 0..50u32 { produce.call(&mut store, i)?; }
            println!("[ShuffleRRHeavyTest] 50 producers wrote 150 records each to slots 0-49.");

            // Step 2: reversed upstream list [49,48,...,0] — call-order determines slot assignment
            let upstreams:   Vec<usize> = (0..50).rev().collect();  // [49,48,..,0]
            let downstreams: Vec<usize> = (50..60).collect();
            let splice_addr = store.data().splice_addr;
            ShuffleConnection::new(&upstreams, &downstreams, RoundRobinPartition::new())
                .bridge(splice_addr);
            println!("[ShuffleRRHeavyTest] bridge([49..0] → [50..59], RoundRobin) done.");
            println!("[ShuffleRRHeavyTest] Expected per slot: 750 records; \
                      slot 50 first record = p=49 (not p=0 as in Modulo).");

            // Step 3: verify all 10 downstream slots
            println!("[ShuffleRRHeavyTest] Downstream summary (expect 750 records each):");
            let base_ptr = memory.data_ptr(&store);
            let mut all_ok = true;
            for slot in 50u32..60 {
                let n = count.call(&mut store, slot)?;
                let ok = n == 750;
                all_ok &= ok;
                println!("  slot {:2}: {:4} records  {}", slot, n, if ok { "✓" } else { "FAIL" });
            }

            // Step 4: spot-check slot 50 — first record should be from producer 49
            let packed = dump.call(&mut store, 50)?;
            if packed > 0 {
                let ptr = (packed >> 32) as usize;
                let len = (packed & 0xFFFF_FFFF) as usize;
                let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                let s   = String::from_utf8_lossy(raw);
                let lines: Vec<&str> = s.lines().collect();
                let total = lines.len();
                println!("[ShuffleRRHeavyTest] Spot-check slot 50 (expect first record p=49):");
                for (i, l) in lines.iter().take(3).enumerate() { println!("  [{:3}] {}", i, l); }
                if total > 6 { println!("  ... {} records omitted ...", total - 6); }
                for i in total.saturating_sub(3)..total { println!("  [{:3}] {}", i, lines[i]); }
            }
            println!("[ShuffleRRHeavyTest] Overall: {}", if all_ok { "✓ ALL PASS" } else { "FAIL" });
            println!("[ShuffleRRHeavyTest] Done.\n");
        }

        // ─────────────────────────────────────────────────────────────────
        // Routing test (heavy): ShuffleConnection — FixedMapPartition, uneven 50→2
        //
        // An asymmetric fixed-range split across 2 downstream slots:
        //   IDs  0-29 (30 producers) → slot_index 0 → slot 50  (4500 records)
        //   IDs 30-49 (20 producers) → slot_index 1 → slot 51  (3000 records)
        //
        // Unlike Modulo/RoundRobin (which balance evenly), FixedMap expresses
        // arbitrary topology — here a 60/40 split.  Each group (30 and 20
        // upstreams) is independently within PARALLEL_THRESHOLD so both are
        // merged sequentially; the two groups run concurrently via thread::scope.
        //
        // Verification:
        //   slot 50: 4500 records  (first record p=0,  last record p=29,s=0149)
        //   slot 51: 3000 records  (first record p=30, last record p=49,s=0149)
        // ─────────────────────────────────────────────────────────────────
        "shuffle_fixedmap_heavy_test" => {
            println!("\n[ShuffleFMHeavyTest] === FixedMapPartition uneven 50→2 (150 records × 50 producers) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream_heavy")?;
            let count   = instance.get_typed_func::<u32, u32>(&mut store, "count_stream_records")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            // Step 1: 50 producers write to slots 0-49
            for i in 0..50u32 { produce.call(&mut store, i)?; }
            println!("[ShuffleFMHeavyTest] 50 producers wrote 150 records each to slots 0-49.");

            // Step 2: asymmetric fixed-range split — IDs 0-29 → slot 50, IDs 30-49 → slot 51
            let upstreams:   Vec<usize> = (0..50).collect();
            let downstreams: Vec<usize> = vec![50, 51];
            let map: std::collections::HashMap<usize, usize> =
                (0..50).map(|i| (i, if i < 30 { 0 } else { 1 })).collect();
            let splice_addr = store.data().splice_addr;
            ShuffleConnection::new(
                &upstreams, &downstreams,
                FixedMapPartition::new(map, 0),
            ).bridge(splice_addr);
            println!("[ShuffleFMHeavyTest] bridge([0..49] → [50,51], FixedMap{{0-29→50, 30-49→51}}) done.");

            // Step 3: verify both downstream slots
            let base_ptr = memory.data_ptr(&store);
            let checks = [(50u32, 30u32 * 150, 0u32, 29u32),   // slot, expected_count, first_p, last_p
                          (51u32, 20u32 * 150, 30u32, 49u32)];
            for (slot, expected, first_p, last_p) in checks {
                let n = count.call(&mut store, slot)?;
                println!("[ShuffleFMHeavyTest] Slot {}: {} records (expected {}) {}",
                    slot, n, expected, if n == expected { "✓" } else { "FAIL" });
                let packed = dump.call(&mut store, slot)?;
                if packed > 0 {
                    let ptr = (packed >> 32) as usize;
                    let len = (packed & 0xFFFF_FFFF) as usize;
                    let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                    let s   = String::from_utf8_lossy(raw);
                    let lines: Vec<&str> = s.lines().collect();
                    let total = lines.len();
                    println!("[ShuffleFMHeavyTest]   First 3 (expect p={}):", first_p);
                    for (i, l) in lines.iter().take(3).enumerate() { println!("    [{:4}] {}", i, l); }
                    if total > 6 { println!("    ... {} records omitted ...", total - 6); }
                    println!("[ShuffleFMHeavyTest]   Last 3 (expect p={}):", last_p);
                    for i in total.saturating_sub(3)..total { println!("    [{:4}] {}", i, lines[i]); }
                }
            }
            println!("[ShuffleFMHeavyTest] Done.\n");
        }

        // ─────────────────────────────────────────────────────────────────
        // Routing test (heavy): BroadcastConnection 20→10
        //
        // 20 producers each write 150 records (~70 bytes each) to slots 0-19,
        // producing 2-3 page chains per upstream slot (~147 KB total).
        //
        // BroadcastConnection fans every upstream into every downstream:
        //   slots 20-29 each receive the full merged chain from all 20 producers
        //   → 20 × 150 = 3000 records per downstream slot
        //
        // Verification:
        //   all 10 downstream slots: 3000 records each
        //   spot-checks on slots 20 and 29: first record p=0,s=0000;
        //                                   last  record p=19,s=0149
        // ─────────────────────────────────────────────────────────────────
        "broadcast_heavy_test" => {
            println!("\n[BroadcastHeavyTest] === BroadcastConnection 20→10 (150 records × 20 producers) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream_heavy")?;
            let count   = instance.get_typed_func::<u32, u32>(&mut store, "count_stream_records")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            // Step 1: 20 producers write to slots 0-19
            for i in 0..20u32 { produce.call(&mut store, i)?; }
            println!("[BroadcastHeavyTest] 20 producers wrote 150 records each to slots 0-19  \
                      (~{} KB total).", 20 * 150 * 70 / 1024);

            // Step 2: broadcast all 20 upstreams into all 10 downstream slots
            let upstreams:   Vec<usize> = (0..20).collect();
            let downstreams: Vec<usize> = (20..30).collect();
            let splice_addr = store.data().splice_addr;
            BroadcastConnection::new(&upstreams, &downstreams).bridge(splice_addr);
            println!("[BroadcastHeavyTest] BroadcastConnection::bridge([0..19] → [20..29]) done.");

            // Step 3: verify all 10 downstream slots — each must hold exactly 3000 records
            println!("[BroadcastHeavyTest] Downstream summary (expect 3000 records each):");
            let base_ptr = memory.data_ptr(&store);
            let mut all_ok = true;
            for slot in 20u32..30 {
                let n = count.call(&mut store, slot)?;
                let ok = n == 3000;
                all_ok &= ok;
                println!("  slot {:2}: {:4} records  {}", slot, n, if ok { "✓" } else { "FAIL" });
            }

            // Step 4: spot-check first (slot 20) and last (slot 29) downstream —
            //         both must start at p=0,s=0000 and end at p=19,s=0149
            for spot in [20u32, 29u32] {
                let packed = dump.call(&mut store, spot)?;
                if packed > 0 {
                    let ptr = (packed >> 32) as usize;
                    let len = (packed & 0xFFFF_FFFF) as usize;
                    let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                    let s   = String::from_utf8_lossy(raw);
                    let lines: Vec<&str> = s.lines().collect();
                    let total = lines.len();
                    println!("[BroadcastHeavyTest] Spot-check slot {} (expect first=p=0,s=0000 / last=p=19,s=0149):", spot);
                    for (i, l) in lines.iter().take(3).enumerate() { println!("  [{:3}] {}", i, l); }
                    if total > 6 { println!("  ... {} records omitted ...", total - 6); }
                    for i in total.saturating_sub(3)..total { println!("  [{:3}] {}", i, lines[i]); }
                }
            }
            println!("[BroadcastHeavyTest] Overall: {}", if all_ok { "✓ ALL PASS" } else { "FAIL" });
            println!("[BroadcastHeavyTest] Done.\n");
        }

        _ => eprintln!("Unknown role: {}", role),
    }

   
    if role == "func_a" {
        println!("[System] Triggering Post-Execution Organization...");
        let organizer = BucketOrganizer::new(&mut store, &memory);
        unsafe {
            // organizer.consume_all_buckets(LastWriteWinsPolicy);
            // organizer.consume_all_buckets(MinIdWinsPolicy);
            organizer.consume_all_buckets(MaxIdWinsPolicy);
        }
        println!("[System] Organization & FDT Registration Completed.");
    }

    Ok(())
}
