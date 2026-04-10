use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::sync::atomic::Ordering;
use wasmtime::*;

use crate::shm::{expand_mapping, map_into_memory};

use common::{RegistryEntry, Superblock, REGISTRY_OFFSET, TARGET_OFFSET};

pub struct WorkerState {
    pub file: File,
    pub splice_addr: usize,
}

/// Creates a Wasmtime engine configured for shared memory:
/// 4 GiB static address space, no guard pages (VMA is managed manually), threads enabled.
pub fn create_wasmtime_engine() -> Result<Engine> {
    let mut config = Config::new();
    // Allow full 4 GiB address space
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

    let memory_handle = memory;

    linker.func_wrap(
        "env",
        "host_resolve_atomic",
        move |caller: Caller<'_, WorkerState>, ptr: u32, len: u32| -> u32 {
            let base_ptr = memory_handle.data_ptr(&caller);
            let name_bytes =
                unsafe { std::slice::from_raw_parts(base_ptr.add(ptr as usize), len as usize) };

            let name_len = std::cmp::min(name_bytes.len(), 52);
            let mut entry_name = [0u8; 52];
            entry_name[..name_len].copy_from_slice(&name_bytes[..name_len]);

            let host_base = unsafe { memory_handle.data_ptr(&caller).add(TARGET_OFFSET as usize) };
            let superblock = unsafe { &*(host_base as *const Superblock) };
            let lock = &superblock.registry_lock;
            let registry_base = unsafe { host_base.add(REGISTRY_OFFSET as usize) };

            // Spinlock acquire
            while lock
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                std::hint::spin_loop();
            }

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
                            payload_offset: common::AtomicShmOffset::new(0),
                            payload_len: core::sync::atomic::AtomicU32::new(0),
                        },
                    );
                }

                next_idx_atomic.store(current_count + 1, Ordering::Relaxed);
            }

            // Spinlock release
            lock.store(0, Ordering::Release);

            result_index
        },
    )?;

    // ── Intra-wave barrier (futex-backed) ───────────────────────────────────
    //
    // Guest processes in the same wave call `host_barrier_wait(barrier_id, party_count)`.
    // The implementation uses Linux futex so waiters sleep (zero CPU) until the
    // last party arrives and wakes them.  Cross-process because all processes
    // mmap the same SHM file.
    linker.func_wrap(
        "env",
        "host_barrier_wait",
        |caller: Caller<'_, WorkerState>, barrier_id: u32, party_count: u32| {
            let splice_addr = caller.data().splice_addr;
            let sb = unsafe { &*(splice_addr as *const Superblock) };
            let barrier = &sb.barriers[barrier_id as usize];

            // Arrive: atomically increment the counter.
            let arrived = barrier.fetch_add(1, Ordering::AcqRel) + 1;

            if arrived >= party_count {
                // Last to arrive — wake all sleeping waiters.
                unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        barrier as *const _ as *const libc::c_int,
                        libc::FUTEX_WAKE,
                        i32::MAX,
                        std::ptr::null::<libc::timespec>(),
                    );
                }
            } else {
                // Sleep until the counter reaches party_count.
                loop {
                    let current = barrier.load(Ordering::Acquire);
                    if current >= party_count {
                        break;
                    }
                    // futex(FUTEX_WAIT): sleep only if *barrier == current
                    // (guards against lost wakeups and spurious returns).
                    unsafe {
                        libc::syscall(
                            libc::SYS_futex,
                            barrier as *const _ as *const libc::c_int,
                            libc::FUTEX_WAIT,
                            current as libc::c_int,
                            std::ptr::null::<libc::timespec>(),
                        );
                    }
                }
            }
        },
    )?;

    linker.define(&mut *store, "env", "memory", memory)?;

    // ── WASI stubs ────────────────────────────────────────────────────────────
    // py_guest embeds MicroPython which links against wasi-libc and imports a
    // handful of fd_* functions.  We provide no-op stubs so the module
    // instantiates without a full WASI context.  Python output is discarded
    // (workloads write results via shm.write_output instead of print()).
    linker.func_wrap(
        "wasi_snapshot_preview1", "fd_write",
        |_caller: Caller<'_, WorkerState>,
         _fd: i32, _iovs: i32, _iovs_len: i32, _nwritten_ptr: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1", "fd_close",
        |_caller: Caller<'_, WorkerState>, _fd: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1", "fd_seek",
        |_caller: Caller<'_, WorkerState>,
         _fd: i32, _offset: i64, _whence: i32, _newoffset_ptr: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1", "fd_fdstat_get",
        |_caller: Caller<'_, WorkerState>, _fd: i32, _stat_ptr: i32| -> i32 {
            8 // WASI errno: EBADF — no real file descriptors available
        },
    )?;

    Ok(memory)
}

/// Loads the guest WASM module and enters a persistent call loop.
///
/// Reads lines from stdin with format `"<arg0> <arg1>\n"`, calls
/// `func(arg0, arg1) -> ()` for each, and writes `"ok\n"` (or `"err: …\n"`)
/// to stdout.  Exits cleanly when stdin is closed (EOF).
///
/// Used by `WasmLoopWorker` in the DAG runner's `pipeline.rs` and `grouping.rs`.
pub fn run_wasm_loop(shm_path: &str, wasm_path: &str, func: &str) -> Result<()> {
    use std::io::{BufRead, Write};

    let file = OpenOptions::new().read(true).write(true).open(shm_path)?;
    let engine = create_wasmtime_engine()?;
    let mut store = Store::new(&engine, WorkerState {
        file: file.try_clone()?,
        splice_addr: 0,
    });
    let mut linker = Linker::new(&engine);
    setup_vma_environment(&mut store, &mut linker, &file)?;
    let module = Module::from_file(&engine, wasm_path)?;
    let instance = linker.instantiate(&mut store, &module)?;
    let f = instance
        .get_typed_func::<(u32, u32), ()>(&mut store, func)
        .map_err(|e| anyhow::anyhow!("no export '{}': {}", func, e))?;

    let stdin  = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line?;
        let mut parts = line.split_whitespace();
        let arg0: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let arg1: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        match f.call(&mut store, (arg0, arg1)) {
            Ok(())  => writeln!(out, "ok")?,
            Err(e)  => writeln!(out, "err: {}", e)?,
        }
        out.flush()?;
    }
    Ok(())
}

/// Loads the guest WASM module, calls `func` with the given return type, and exits.
/// `ret_type` is one of `"void"`, `"void2"`, `"u32"`, or `"fatptr"`.
/// `"void2"` calls `func(arg, arg1)` — used by StreamPipeline stages.
/// Called by the DAG runner as a subprocess for each WASM node.
pub fn run_wasm_call(shm_path: &str, wasm_path: &str, func: &str, ret_type: &str, arg: u32, arg1: Option<u32>) -> Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(shm_path)?;
    let engine = create_wasmtime_engine()?;
    let mut store = Store::new(&engine, WorkerState {
        file: file.try_clone()?,
        splice_addr: 0,
    });
    let mut linker = Linker::new(&engine);
    let memory = setup_vma_environment(&mut store, &mut linker, &file)?;
    let module = Module::from_file(&engine, wasm_path)?;
    let instance = linker.instantiate(&mut store, &module)?;

    match ret_type {
        "void2" => {
            let a1 = arg1.unwrap_or(0);
            let f = instance.get_typed_func::<(u32, u32), ()>(&mut store, func)
                .map_err(|e| anyhow::anyhow!("no export '{}': {}", func, e))?;
            f.call(&mut store, (arg, a1))?;
        }
        "u32" => {
            let f = instance.get_typed_func::<u32, u32>(&mut store, func)
                .map_err(|e| anyhow::anyhow!("no export '{}': {}", func, e))?;
            let result = f.call(&mut store, arg)?;
            println!("  {}({}) → {}", func, arg, result);
        }
        "fatptr" => {
            let f = instance.get_typed_func::<u32, u64>(&mut store, func)
                .map_err(|e| anyhow::anyhow!("no export '{}': {}", func, e))?;
            let packed = f.call(&mut store, arg)?;
            if packed > 0 {
                let ptr = (packed >> 32) as usize;
                let len = (packed & 0xFFFF_FFFF) as usize;
                let base_ptr = memory.data_ptr(&store);
                let raw = unsafe { std::slice::from_raw_parts(base_ptr.add(ptr), len) };
                print!("{}", String::from_utf8_lossy(raw));
            }
        }
        _ => {
            let f = instance.get_typed_func::<u32, ()>(&mut store, func)
                .map_err(|e| anyhow::anyhow!("no export '{}': {}", func, e))?;
            f.call(&mut store, arg)?;
        }
    }
    Ok(())
}
