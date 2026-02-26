mod manager;
mod shm;
mod worker;
mod organizer; 
mod policy;   


use anyhow::Result;
use std::env;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    
    if args.len() > 1 {
        let role = &args[1];
        let shm_path = &args[2];
        let id: u32 = args[3].parse().unwrap_or(0);
        worker::run_worker(role, shm_path, id)
    } else {
        manager::run_manager()
    }
}