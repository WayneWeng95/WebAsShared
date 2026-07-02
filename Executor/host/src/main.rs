mod shm;
mod runtime;
mod routing;
mod policy;
mod shard;

use anyhow::Result;
use std::env;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 && args[1] == "compile" {
        // AOT-compile a guest .wasm into a .cwasm: ./host compile <in.wasm> [out.cwasm]
        // The .cwasm deserializes much faster than JIT-compiling, cutting the
        // per-subprocess cold-start cost paid by every WASM worker.
        let wasm_path = args.get(2).map(String::as_str)
            .unwrap_or_else(|| { eprintln!("usage: host compile <in.wasm> [out.cwasm]"); std::process::exit(2) });
        let out_path = args.get(3).cloned()
            .unwrap_or_else(|| wasm_path.trim_end_matches(".wasm").to_string() + ".cwasm");
        runtime::worker::precompile_guest(wasm_path, &out_path)
    } else if args.len() > 1 && args[1] == "dag" {
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
    } else if args.len() > 1 && args[1] == "split" {
        // Native big-file split (no WASM, no 4 GiB cap): host split <input> <N> <out_prefix>
        // Writes <out_prefix>.0 .. <out_prefix>.{N-1}, cutting on line boundaries.
        if args.len() < 5 {
            eprintln!("usage: host split <input> <N> <out_prefix>");
            std::process::exit(2);
        }
        let input = &args[2];
        let n: usize = args[3].parse()
            .unwrap_or_else(|_| { eprintln!("split: N must be a positive integer"); std::process::exit(2) });
        let out_prefix = &args[4];
        shard::run_split(input, n, out_prefix)
    } else if args.len() > 1 && args[1] == "merge" {
        // Native partial merge (no WASM, no 4 GiB cap):
        //   host merge <reducer> <output> <partial...>
        if args.len() < 5 {
            eprintln!("usage: host merge <reducer> <output> <partial...>  (reducers: wordcount, counters, concat)");
            std::process::exit(2);
        }
        let reducer = &args[2];
        let output = &args[3];
        let partials = &args[4..];
        shard::run_merge(reducer, output, partials)
    } else if args.len() > 1 && args[1] == "wasm-loop" {
        // Persistent pipeline worker: ./host wasm-loop <shm_path> <wasm_path> <func>
        // Reads "arg0 arg1\n" lines from stdin, calls func(arg0, arg1) for each,
        // writes "ok\n" back.  Exits when stdin is closed (EOF).
        let shm_path  = args.get(2).map(String::as_str).unwrap_or("");
        let wasm_path = args.get(3).map(String::as_str).unwrap_or(common::WASM_PATH);
        let func      = args.get(4).map(String::as_str).unwrap_or("");
        runtime::worker::run_wasm_loop(shm_path, wasm_path, func)
    } else if args.len() > 1 {
        let role = &args[1];
        let shm_path = &args[2];
        let id: u32 = args[3].parse().unwrap_or(0);
        runtime::test::run_worker(role, shm_path, id)
    } else {
        runtime::manager::run_manager()
    }
}
