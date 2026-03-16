mod shm;
mod runtime;
mod routing;
mod policy;

use anyhow::Result;
use std::env;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 && args[1] == "dag" {
        // DAG mode: ./host dag <json_file>
        let json_path = args.get(2).map(String::as_str).unwrap_or("dag.json");
        runtime::dag_runner::run_dag_file(json_path)
    } else if args.len() > 1 && args[1] == "wasm-call" {
        // Subprocess WASM worker: ./host wasm-call <shm_path> <wasm_path> <func> <ret_type> <arg> [arg1]
        let shm_path  = args.get(2).map(String::as_str).unwrap_or("");
        let wasm_path = args.get(3).map(String::as_str).unwrap_or(common::WASM_PATH);
        let func      = args.get(4).map(String::as_str).unwrap_or("");
        let ret_type  = args.get(5).map(String::as_str).unwrap_or("void");
        let arg: u32  = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(0);
        let arg1: Option<u32> = args.get(7).and_then(|s| s.parse().ok());
        runtime::worker::run_wasm_call(shm_path, wasm_path, func, ret_type, arg, arg1)
    } else if args.len() > 1 {
        let role = &args[1];
        let shm_path = &args[2];
        let id: u32 = args[3].parse().unwrap_or(0);
        runtime::worker::run_worker(role, shm_path, id)
    } else {
        runtime::manager::run_manager()
    }
}
