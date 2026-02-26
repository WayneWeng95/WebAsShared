use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;
use wasmtime::*;

use crate::shm::{expand_mapping, map_into_memory};

use common::{RegistryEntry, INITIAL_SHM_SIZE, REGISTRY_OFFSET, REGISTRY_SIZE, TARGET_OFFSET};

use crate::organizer::BucketOrganizer;
use crate::policy::{LastWriteWinsPolicy, MajorityWinsPolicy, MaxIdWinsPolicy, MinIdWinsPolicy};

const WASM_PATH: &str = "../target/wasm32-unknown-unknown/release/guest.wasm";

// Config constants
const WRITER_TOTAL_ITEMS: u32 = 5000;
const READER_TOTAL_POLLS: u32 = 15;
const READER_POLL_INTERVAL_MS: u64 = 200;

pub struct WorkerState {
    pub file: File,
    pub splice_addr: usize,
}

/// WASM engine config
/// Now isolated to handle engine creation and configuration
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

/// VMA environment adjust
/// Handles Memory creation, File mapping, and Host Function exports
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

            let lock_ptr = unsafe { host_base.add(16) as *const AtomicU32 };
            let next_idx_ptr = unsafe { host_base.add(20) as *const AtomicU32 };
            let registry_base = unsafe { host_base.add(REGISTRY_OFFSET as usize) };

            let lock = unsafe { &*lock_ptr };

            // --- Spinlock Acquire ---
            while lock
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                std::hint::spin_loop();
            }

            // --- Search Registry ---
            let mut result_index = u32::MAX;
            let next_idx_atomic = unsafe { &*next_idx_ptr };
            let current_count = next_idx_atomic.load(Ordering::Relaxed);

            for i in 0..current_count {
                let entry_ptr =
                    unsafe { registry_base.add(i as usize * 64) as *const RegistryEntry };
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

            
            let log_offset_ptr = unsafe { target_base.add(12) as *const AtomicU32 };
            let log_offset = unsafe { (*log_offset_ptr).load(Ordering::Acquire) };

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
