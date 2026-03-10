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
    } else if args.len() > 1 {
        let role = &args[1];
        let shm_path = &args[2];
        let id: u32 = args[3].parse().unwrap_or(0);
        runtime::worker::run_worker(role, shm_path, id)
    } else {
        runtime::manager::run_manager()
    }
}
