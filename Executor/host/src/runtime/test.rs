// Legacy integration-test worker roles, invoked as subprocesses by manager.rs.
//
// `run_worker` dispatches on a named role string and exercises the WASM guest
// functions and host routing APIs end-to-end.  Every role is spawned as a
// separate subprocess by `run_manager`.  Not used by the DAG runner.
//
// Roles:
//   writer / reader     — basic SHM read/write phase (uses BucketOrganizer after writes)
//   func_a / func_b     — sequential WASM call phase (uses BucketOrganizer after func_a)
//   stream_bridge_test  — StreamBridge 1→1 bridge
//   aggregate_test      — AggregateConnection N→1 merge
//   shuffle_test        — ShuffleConnection ModuloPartition (light)
//   shuffle_roundrobin_test    — ShuffleConnection RoundRobinPartition (light)
//   shuffle_fixedmap_test      — ShuffleConnection FixedMapPartition (light)
//   shuffle_heavy_test         — ShuffleConnection ModuloPartition 50→10 (heavy)
//   shuffle_roundrobin_heavy_test — ShuffleConnection RoundRobinPartition 50→10 (heavy)
//   shuffle_fixedmap_heavy_test   — ShuffleConnection FixedMapPartition 50→2 (heavy)
//   broadcast_heavy_test       — BroadcastConnection 20→10 (heavy)

use anyhow::Result;
use std::fs::OpenOptions;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use wasmtime::*;

use common::{Superblock, REGISTRY_OFFSET, REGISTRY_SIZE, TARGET_OFFSET, WASM_PATH};

use crate::runtime::mem_operation::organizer::BucketOrganizer;
use crate::policy::{MajorityWinsPolicy, MaxIdWinsPolicy};
use crate::routing::aggregate::AggregateConnection;
use crate::routing::broadcast::BroadcastConnection;
use crate::routing::shuffle::ShuffleConnection;
use crate::routing::stream::StreamBridge;
use crate::policy::{FixedMapPartition, ModuloPartition, RoundRobinPartition};

use super::worker::{WorkerState, create_wasmtime_engine, setup_vma_environment};

// Config constants for the basic read/write phase.
const WRITER_TOTAL_ITEMS: u32 = 5000;
const READER_TOTAL_POLLS: u32 = 15;
const READER_POLL_INTERVAL_MS: u64 = 200;

/// Loads the guest WASM module and executes the function for `role` with the given `id`.
/// Supported roles: `"writer"`, `"reader"`, `"func_a"`, `"func_b"`, and all routing test roles.
/// After writers complete, triggers the `BucketOrganizer` to resolve conflicts and GC pages.
pub fn run_worker(role: &str, shm_path: &str, id: u32) -> Result<()> {
    let pid = std::process::id();
    let file = OpenOptions::new().read(true).write(true).open(shm_path)?;

    let engine = create_wasmtime_engine()?;

    let mut store = Store::new(
        &engine,
        WorkerState {
            file: file.try_clone()?,
            splice_addr: 0,
        },
    );

    let mut linker = Linker::new(&engine);

    let memory = setup_vma_environment(&mut store, &mut linker, &file)?;

    let module = Module::from_file(&engine, WASM_PATH)?;
    let instance = linker.instantiate(&mut store, &module)?;

    if role == "writer" {
        let func = instance.get_typed_func::<u32, ()>(&mut store, "writer")?;
        println!("[Writer ID:{}] (PID:{}) Started...", id, pid);
        for _i in 1..=WRITER_TOTAL_ITEMS {
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
        // Routing test: StreamBridge 1→1 bridge
        //
        // Producer 0 writes 3 records to stream slot 0.
        // Host calls StreamBridge::bridge(0 → 1): two atomic stores redirect
        // slot 1's head/tail to slot 0's existing page chain (zero copy).
        // WASM consumer reads from slot 1 — the last record must come from
        // producer 0, proving the bridge wired the chains correctly.
        // ─────────────────────────────────────────────────────────────────
        "stream_bridge_test" => {
            println!("\n[StreamBridgeTest] === StreamBridge 1→1 bridge ===");
            let produce  = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream")?;
            let consume  = instance.get_typed_func::<u32, u64>(&mut store, "consume_routed_stream")?;

            produce.call(&mut store, 0)?;
            println!("[StreamBridgeTest] Producer 0 wrote 3 records to slot 0.");

            let splice_addr = store.data().splice_addr;
            let bridged = StreamBridge::new(splice_addr).bridge(0, 1);
            println!("[StreamBridgeTest] StreamBridge::bridge(0 → 1): {}",
                if bridged { "OK" } else { "FAIL (slot 0 was empty)" });

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
        // ─────────────────────────────────────────────────────────────────
        "aggregate_test" => {
            println!("\n[AggregateTest] === AggregateConnection N→1 merge ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            for i in 0..3u32 {
                produce.call(&mut store, i)?;
                println!("[AggregateTest] Producer {} wrote 3 records to slot {}.", i, i);
            }

            let splice_addr = store.data().splice_addr;
            AggregateConnection::new(&[0, 1, 2], 3).bridge(splice_addr);
            println!("[AggregateTest] AggregateConnection::bridge([0,1,2] → 3) done.");

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
        // ─────────────────────────────────────────────────────────────────
        "shuffle_heavy_test" => {
            println!("\n[ShuffleHeavyTest] === ModuloPartition 50→10 (150 records × 50 producers) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream_heavy")?;
            let count   = instance.get_typed_func::<u32, u32>(&mut store, "count_stream_records")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            for i in 0..50u32 { produce.call(&mut store, i)?; }
            println!("[ShuffleHeavyTest] 50 producers wrote 150 records each to slots 0-49  \
                      (~{} KB total).", 50 * 150 * 70 / 1024);

            let upstreams:   Vec<usize> = (0..50).collect();
            let downstreams: Vec<usize> = (50..60).collect();
            let splice_addr = store.data().splice_addr;
            ShuffleConnection::new(&upstreams, &downstreams, ModuloPartition).bridge(splice_addr);
            println!("[ShuffleHeavyTest] bridge([0..49] → [50..59], ModuloPartition(% 10)) done.");

            println!("[ShuffleHeavyTest] Downstream summary (expect 750 records each):");
            let base_ptr = memory.data_ptr(&store);
            let mut all_ok = true;
            for slot in 50u32..60 {
                let n = count.call(&mut store, slot)?;
                let ok = n == 750;
                all_ok &= ok;
                println!("  slot {:2}: {:4} records  {}", slot, n, if ok { "✓" } else { "FAIL" });
            }

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
        // ─────────────────────────────────────────────────────────────────
        "shuffle_roundrobin_heavy_test" => {
            println!("\n[ShuffleRRHeavyTest] === RoundRobinPartition 50→10 reversed (150 records × 50 producers) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream_heavy")?;
            let count   = instance.get_typed_func::<u32, u32>(&mut store, "count_stream_records")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            for i in 0..50u32 { produce.call(&mut store, i)?; }
            println!("[ShuffleRRHeavyTest] 50 producers wrote 150 records each to slots 0-49.");

            let upstreams:   Vec<usize> = (0..50).rev().collect();
            let downstreams: Vec<usize> = (50..60).collect();
            let splice_addr = store.data().splice_addr;
            ShuffleConnection::new(&upstreams, &downstreams, RoundRobinPartition::new())
                .bridge(splice_addr);
            println!("[ShuffleRRHeavyTest] bridge([49..0] → [50..59], RoundRobin) done.");
            println!("[ShuffleRRHeavyTest] Expected per slot: 750 records; \
                      slot 50 first record = p=49 (not p=0 as in Modulo).");

            println!("[ShuffleRRHeavyTest] Downstream summary (expect 750 records each):");
            let base_ptr = memory.data_ptr(&store);
            let mut all_ok = true;
            for slot in 50u32..60 {
                let n = count.call(&mut store, slot)?;
                let ok = n == 750;
                all_ok &= ok;
                println!("  slot {:2}: {:4} records  {}", slot, n, if ok { "✓" } else { "FAIL" });
            }

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
        // Routing test (heavy): ShuffleConnection — FixedMapPartition, 50→2 uneven
        // ─────────────────────────────────────────────────────────────────
        "shuffle_fixedmap_heavy_test" => {
            println!("\n[ShuffleFMHeavyTest] === FixedMapPartition uneven 50→2 (150 records × 50 producers) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream_heavy")?;
            let count   = instance.get_typed_func::<u32, u32>(&mut store, "count_stream_records")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            for i in 0..50u32 { produce.call(&mut store, i)?; }
            println!("[ShuffleFMHeavyTest] 50 producers wrote 150 records each to slots 0-49.");

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

            let base_ptr = memory.data_ptr(&store);
            let checks = [(50u32, 30u32 * 150, 0u32, 29u32),
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
        // ─────────────────────────────────────────────────────────────────
        "broadcast_heavy_test" => {
            println!("\n[BroadcastHeavyTest] === BroadcastConnection 20→10 (150 records × 20 producers) ===");
            let produce = instance.get_typed_func::<u32, ()>(&mut store, "produce_stream_heavy")?;
            let count   = instance.get_typed_func::<u32, u32>(&mut store, "count_stream_records")?;
            let dump    = instance.get_typed_func::<u32, u64>(&mut store, "dump_stream_records")?;

            for i in 0..20u32 { produce.call(&mut store, i)?; }
            println!("[BroadcastHeavyTest] 20 producers wrote 150 records each to slots 0-19  \
                      (~{} KB total).", 20 * 150 * 70 / 1024);

            let upstreams:   Vec<usize> = (0..20).collect();
            let downstreams: Vec<usize> = (20..30).collect();
            let splice_addr = store.data().splice_addr;
            BroadcastConnection::new(&upstreams, &downstreams).bridge(splice_addr);
            println!("[BroadcastHeavyTest] BroadcastConnection::bridge([0..19] → [20..29]) done.");

            println!("[BroadcastHeavyTest] Downstream summary (expect 3000 records each):");
            let base_ptr = memory.data_ptr(&store);
            let mut all_ok = true;
            for slot in 20u32..30 {
                let n = count.call(&mut store, slot)?;
                let ok = n == 3000;
                all_ok &= ok;
                println!("  slot {:2}: {:4} records  {}", slot, n, if ok { "✓" } else { "FAIL" });
            }

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
            organizer.consume_all_buckets(MaxIdWinsPolicy);
        }
        println!("[System] Organization & FDT Registration Completed.");
    }

    Ok(())
}
