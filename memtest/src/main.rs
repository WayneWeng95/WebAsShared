use shared_memory::*;
use std::env;
use std::process::{Command, Stdio};
use wasmtime::*;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();

    // --- LOGIC SWITCH: Am I the Agent or the Worker? ---
    if args.len() > 1 && args[1] == "--worker" {
        run_worker_node(&args[2], &args[3]) // Args: shmem_id, mode
    } else {
        run_orchestrator_agent()
    }
}

/// --- THE ORCHESTRATOR (Agent) ---
/// Manages the OS resources and launches child processes
fn run_orchestrator_agent() -> anyhow::Result<()> {
    let shmem_id = "wasm_os_bus";
    let shmem_size = 65536; // 64KB

    println!("[Agent] Provisioning OS Shared Memory: {}", shmem_id);
    
    // 1. Create the standalone OS memory segment
    // It exists as long as this '_shmem' handle exists in the Agent.
    let _shmem = ShmemConf::new()
        .os_id(shmem_id)
        .size(shmem_size)
        .create()?;

    // 2. Launch Workers via the management function
    println!("[Agent] Launching Worker A (Writer)...");
    let mut worker_a = launch_worker_process(shmem_id, "writer")?;

    println!("[Agent] Launching Worker B (Reader)...");
    let mut worker_b = launch_worker_process(shmem_id, "reader")?;

    // 3. Wait for them to finish
    worker_a.wait()?;
    worker_b.wait()?;

    println!("[Agent] All workers finished. Shutting down.");
    Ok(())
}

/// --- THE MANAGEMENT FUNCTION ---
fn launch_worker_process(shmem_id: &str, mode: &str) -> std::io::Result<std::process::Child> {
    let exe = env::current_exe()?;
    Command::new(exe)
        .arg("--worker")
        .arg(shmem_id)
        .arg(mode)
        .stdout(Stdio::inherit()) // See worker output in the same terminal
        .spawn()
}

/// --- THE WORKER NODE ---
/// Runs in a separate OS process with its own Wasmtime Engine
fn run_worker_node(shmem_id: &str, mode: &str) -> anyhow::Result<()> {
    println!("[Worker:{}] Connecting to bus: {}", mode, shmem_id);

    // 1. Open the existing OS-level shared memory
    let shmem = ShmemConf::new().os_id(shmem_id).open()?;
    let raw_ptr = shmem.as_ptr();

    // 2. Setup isolated Wasmtime environment
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());

    // 3. Sophisticated Access: Map raw OS pointer to Wasm Memory
    // Note: In a real "Refined" case, the Agent could tell us to map this Read-Only
    let memory_type = MemoryType::new(1, Some(1));
    let wasm_memory = Memory::new(&mut store, memory_type)?;

    // 4. Manual Linking
    let mut linker = Linker::new(&engine);
    linker.define(&mut store, "env", "memory", wasm_memory)?;

    // 5. Mock Wasm Execution (Logic depends on mode)
    // In a real app, you would load a .wasm file here.
    if mode == "writer" {
        println!("[Worker:Writer] Writing 777 to shared memory...");
        unsafe { *(raw_ptr as *mut i32) = 777; }
    } else {
        // Simple polling for the data
        println!("[Worker:Reader] Polling for data...");
        loop {
            let val = unsafe { *(raw_ptr as *const i32) };
            if val == 777 {
                println!("[Worker:Reader] Detected data change! Value is: {}", val);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    Ok(())
}