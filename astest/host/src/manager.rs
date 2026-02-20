use anyhow::Result;
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::shm::format_shared_memory;

// Used for testing the shared memory read and write functionality

pub fn run_manager() -> Result<()> {
    println!("[Manager] Formatting Initial 16MB Dynamic Shared Memory...");
    
    let shm_path_buf = Path::new("/dev/shm").join("my_serverless_shm");
    let shm_path = shm_path_buf.to_str().unwrap().to_string();

    // Initialize the shared memory
    format_shared_memory(&shm_path)?;

    let myself = env::current_exe()?;

    // Start the reader tasks
    let mut reader_handles = Vec::new(); 
    for i in 0..4 {
        reader_handles.push(Command::new(&myself).arg("reader").arg(&shm_path).arg(i.to_string()).spawn()?);
    }
    thread::sleep(Duration::from_millis(300));

    // Start the writer tasks
    let mut writer_handles = Vec::new();
    for i in 0..4 {
        writer_handles.push(Command::new(&myself).arg("writer").arg(&shm_path).arg(i.to_string()).spawn()?);
    }

    // Wait for all tasks to complete
    for mut handle in writer_handles { handle.wait()?; }
    for mut handle in reader_handles { handle.wait()?; }

    println!("[Manager] Completed.");
    Ok(())
}