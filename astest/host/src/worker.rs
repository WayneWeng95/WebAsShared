use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::thread;
use std::time::Duration;
use wasmtime::*;

use crate::shm::{expand_mapping, map_into_memory, INITIAL_SHM_SIZE, TARGET_OFFSET};

const WASM_PATH: &str = "../target/wasm32-unknown-unknown/release/guest.wasm";

const WRITER_TOTAL_ITEMS: u32 = 500_000;
const WRITER_PROGRESS_STEP: u32 = 100_000;
const READER_TOTAL_POLLS: u32 = 20;
const READER_POLL_INTERVAL_MS: u64 = 200;

pub struct WorkerState {
    pub file: File,
    pub splice_addr: usize,
}

/// WASM engine config
fn create_wasmtime_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.static_memory_maximum_size(1 << 32); 
    config.static_memory_guard_size(0);
    config.dynamic_memory_guard_size(0);
    config.wasm_threads(true); 
    
    Engine::new(&config)
}

/// VMA environment adjust
fn setup_vma_environment(
    store: &mut Store<WorkerState>, 
    linker: &mut Linker<WorkerState>, 
    file: &File
) -> Result<Memory> {
    // This memory region is now hard bind with the WASM runtime safe memory boundary. 
    let memory_ty = MemoryType::shared(49152, 65536); 
    let memory = Memory::new(&mut *store, memory_ty)?;
    
    let base_ptr = memory.data_ptr(&*store);
    let splice_addr = unsafe { base_ptr.add(TARGET_OFFSET) } as usize;
    store.data_mut().splice_addr = splice_addr;

    map_into_memory(file, splice_addr, INITIAL_SHM_SIZE)?;

    linker.func_wrap("env", "host_remap", |mut caller: Caller<'_, WorkerState>, new_size: u32| {
        let state = caller.data_mut();
        println!("[PID:{}] remap to {} MB", std::process::id(), new_size / 1024 / 1024);
        expand_mapping(&state.file, state.splice_addr, new_size as usize).unwrap();
    })?;

    linker.define(&mut *store, "env", "memory", memory)?;
    Ok(memory)
}

pub fn run_worker(role: &str, shm_path: &str, id: u32) -> Result<()> {
    let pid = std::process::id();
    let file = OpenOptions::new().read(true).write(true).open(shm_path)?;
    
    let engine = create_wasmtime_engine()?;
    
    let mut store = Store::new(&engine, WorkerState {
        file: file.try_clone()?,
        splice_addr: 0, 
    });
    
    let mut linker = Linker::new(&engine);
    let memory = setup_vma_environment(&mut store, &mut linker, &file)?;

    let module = Module::from_file(&engine, WASM_PATH)?;
    let instance = linker.instantiate(&mut store, &module)?;

    if role == "writer" {
        let func = instance.get_typed_func::<u32, ()>(&mut store, "writer")?;
        println!("[Writer ID:{}] (PID:{}) Started heavy writing ({}) to trigger expansion...", id, pid, WRITER_TOTAL_ITEMS);
        
        for i in 1..=WRITER_TOTAL_ITEMS {
            func.call(&mut store, id)?; 
            if i % WRITER_PROGRESS_STEP == 0 { 
                println!("[Writer ID:{}] progress: {} / {}", id, i, WRITER_TOTAL_ITEMS); 
            }
        }
        println!("[Writer ID:{}] Finished all writes!", id);
        
    } else if role == "reader" {
        let read_snapshot = instance.get_typed_func::<u32, u64>(&mut store, "reader")?;
        
        let read_live = instance.get_typed_func::<(), u64>(&mut store, "read_live_global")?;
        
        for _ in 0..READER_TOTAL_POLLS {
            let packed_ptr_len = read_snapshot.call(&mut store, id)?;
            let wasm_live = read_live.call(&mut store, ())?;

            let base_ptr = memory.data_ptr(&store);
            let host_direct = unsafe {
                
                let atomic_ptr = base_ptr.add(TARGET_OFFSET + 4096) as *const std::sync::atomic::AtomicU64;
                
                (*atomic_ptr).load(std::sync::atomic::Ordering::SeqCst)
            };
            
            if packed_ptr_len > 0 {
                let ptr = (packed_ptr_len >> 32) as usize;
                let len = (packed_ptr_len & 0xFFFFFFFF) as usize;
                
                let raw_bytes = unsafe { 
                    std::slice::from_raw_parts(base_ptr.add(ptr), len) 
                };
                
                let data_str = String::from_utf8_lossy(raw_bytes);
                
                println!("[Reader ID:{}] (PID:{}) \n  ↳ JSON: {}\n  ↳ Wasm_LIVE: {} | Host_Direct: {}", 
                         id, pid, data_str, wasm_live, host_direct);
            } else {
                println!("[Reader ID:{}] Waiting for Local data... Wasm_LIVE: {} | Host_Direct: {}", 
                         id, wasm_live, host_direct);
            }
            thread::sleep(Duration::from_millis(READER_POLL_INTERVAL_MS));
        }
    }
    Ok(())
}