// use anyhow::Result;
// use std::env;
// use std::fs::OpenOptions;
// use std::io::Write;
// use std::path::Path;
// use std::process::{Command, Stdio};
// use std::thread;
// use std::time::Duration;
// use wasmtime::*;
// use nix::sys::mman::{mmap, MapFlags, ProtFlags};
// use std::num::NonZeroUsize;

// const TARGET_OFFSET: usize = 0x8000_0000;
// const SHM_SIZE: usize = 4096;
// const WASM_PATH: &str = "../target/wasm32-unknown-unknown/release/guest.wasm";

// fn main() -> Result<()> {
//     let args: Vec<String> = env::args().collect();
//     if args.len() > 1 {
//         let role = &args[1];
//         let shm_path = &args[2];
//         let id: u32 = args[3].parse().unwrap_or(0);
//         run_worker(role, shm_path, id)
//     } else {
//         run_manager()
//     }
// }

// fn run_manager() -> Result<()> {
//     println!("[Manager] PID: {} - Starting 4x4 System (Local + Global Atomic)...", std::process::id());

//     let shm_path_buf = Path::new("/dev/shm").join("my_serverless_shm");
//     let shm_path = shm_path_buf.to_str().unwrap().to_string();

//     let mut file = OpenOptions::new()
//         .read(true).write(true).create(true).truncate(true).open(&shm_path_buf)?;

//     file.write_all(&(0u32).to_le_bytes())?;
//     file.set_len(SHM_SIZE as u64)?;

//     let myself = env::current_exe()?;

//     // 1. 启动 4 个 Reader (每个 Reader 都会同时读取 Local 和 Global)
//     let mut reader_handles = Vec::new(); 
//     for i in 0..4 {
//         reader_handles.push(Command::new(&myself)
//             .arg("reader").arg(&shm_path).arg(i.to_string())
//             .stdout(Stdio::inherit()).spawn()?);
//     }

//     thread::sleep(Duration::from_millis(300));

//     // 2. 启动 4 个 Writer
//     let mut writer_handles = Vec::new();
//     for i in 0..4 {
//         writer_handles.push(Command::new(&myself)
//             .arg("writer").arg(&shm_path).arg(i.to_string())
//             .stdout(Stdio::inherit()).spawn()?);
//     }

//     for mut handle in writer_handles { handle.wait()?; }
//     for mut handle in reader_handles { handle.wait()?; }

//     println!("[Manager] All tasks finished.");
//     Ok(())
// }

// fn run_worker(role: &str, shm_path: &str, id: u32) -> Result<()> {
//     let pid = std::process::id();
//     let file = OpenOptions::new().read(true).write(true).open(shm_path)?;
    
//     let mut config = Config::new();
//     config.static_memory_maximum_size(1 << 32);
//     config.static_memory_guard_size(0);
//     config.dynamic_memory_guard_size(0);
//     config.wasm_threads(true); // 必须开启
    
//     let engine = Engine::new(&config)?;
//     let mut store = Store::new(&engine, ());
//     let module = Module::from_file(&engine, WASM_PATH)?;
    
//     // 共享内存类型
//     let memory_ty = MemoryType::shared(17, 16384); 
//     let memory = Memory::new(&mut store, memory_ty)?;

//     let base_ptr = memory.data_ptr(&store);
//     let splice_addr = unsafe { base_ptr.add(TARGET_OFFSET) };
    
//     unsafe {
//         mmap(
//             NonZeroUsize::new(splice_addr as usize),
//             NonZeroUsize::new(SHM_SIZE).unwrap(),
//             ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
//             MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
//             Some(&file),
//             0,
//         )?;
//     }
    
//     let imports = [memory.into()];
//     let instance = Instance::new(&mut store, &module, &imports)?;

//     // ================= 角色执行逻辑 =================
//     if role == "writer" {
//         let func = instance.get_typed_func::<u32, ()>(&mut store, "writer")?;
//         for i in 1..=5 {
//             func.call(&mut store, id)?; 
//             println!("[Writer ID:{}] (PID:{}) wrote Local -> {}, Global -> +1", id, pid, i);
//             thread::sleep(Duration::from_millis(200)); 
//         }
//     } else if role == "reader" {
//         // Reader 同时获取两个 Wasm 函数句柄
//         let read_local = instance.get_typed_func::<u32, u32>(&mut store, "read_local")?;
//         let read_global = instance.get_typed_func::<(), u32>(&mut store, "read_global")?;
        
//         for _ in 0..15 {
//             // 同一个 Wasm 实例，接连调用两个方法
//             let local_val = read_local.call(&mut store, id)?;
//             let global_val = read_global.call(&mut store, ())?;
            
//             // 过滤掉全为 0 的初始刷屏期，让输出更清晰
//             if global_val > 0 {
//                 println!("[Reader ID:{}] (PID:{}) Local: {}, Global Atomic: {}", id, pid, local_val, global_val);
//             }
//             thread::sleep(Duration::from_millis(100));
//         }
//     }

//     Ok(())
// }

