use anyhow::{anyhow, Result};
use super::types::{WasmGroupingParams, PyGroupingParams};
use super::workers::{WasmLoopWorker, PyLoopWorker};

// ─── WasmGrouping executor ────────────────────────────────────────────────────

/// Execute a `WasmGrouping` node.
///
/// One persistent `wasm-loop` subprocess is spawned per stage so the wasmtime
/// JIT compilation and SHM mmap are paid once per stage rather than once per
/// call.  Stages are executed strictly in order: each stage's command is sent
/// and its response awaited before the next stage is dispatched.
///
/// This is the WASM analogue of [`execute_py_grouping`]: both sacrifice
/// intra-stage concurrency in exchange for lower subprocess-startup overhead
/// and a clean single-node DAG representation.
pub(super) fn execute_wasm_grouping(
    params: &WasmGroupingParams,
    node_id: &str,
    shm_path: &str,
    wasm_path: &str,
) -> Result<()> {
    if params.stages.is_empty() {
        return Err(anyhow!("[{}] WasmGrouping has no stages", node_id));
    }

    // Spawn one persistent worker per stage (wasmtime JIT paid once each).
    let mut workers: Vec<WasmLoopWorker> = params.stages.iter()
        .map(|s| WasmLoopWorker::spawn(&s.func, shm_path, wasm_path, node_id))
        .collect::<Result<Vec<_>>>()?;

    println!(
        "  WasmGrouping: {} stages, {} persistent workers (sequential)",
        params.stages.len(),
        params.stages.len()
    );

    // Call each stage in order (no scatter/gather — strictly sequential).
    for (i, stage) in params.stages.iter().enumerate() {
        workers[i].send(stage.arg0, stage.arg1)
            .map_err(|e| anyhow!("[{}] stage {} ({}): send: {}", node_id, i, stage.func, e))?;
        workers[i].recv()
            .map_err(|e| anyhow!("[{}] stage {} ({}): {}", node_id, i, stage.func, e))?;
        println!("    stage {} ({}) done", i, stage.func);
    }

    // Close stdin on all workers (EOF → they exit) and wait for them.
    for w in workers {
        w.finish()?;
    }

    println!("  WasmGrouping done");
    Ok(())
}

// ─── PyGrouping executor ──────────────────────────────────────────────────────

/// Execute a `PyGrouping` node.
///
/// A single persistent `runner.py --loop` process is spawned for the full
/// group.  Each stage sends `"func arg [arg2]\n"` to its stdin and waits for
/// `"ok\n"`.  The process sleeps between calls (blocked on stdin read), so no
/// CPU is consumed between stages.  Closing stdin exits the worker.
///
/// This is a grouping (not a pipeline): all stages run once in strict order
/// with no round-based overlap.  Use `PyPipeline` when multi-round pipelined
/// execution is needed.
pub(super) fn execute_py_grouping(
    params: &PyGroupingParams,
    node_id: &str,
    shm_path: &str,
    python_script: &str,
    python_wasm: Option<&str>,
) -> Result<()> {
    if params.stages.is_empty() {
        return Err(anyhow!("[{}] PyGrouping has no stages", node_id));
    }

    let mut worker = PyLoopWorker::spawn(shm_path, python_script, python_wasm, node_id)?;
    println!("  PyGrouping: {} stages, 1 persistent worker", params.stages.len());

    for (i, stage) in params.stages.iter().enumerate() {
        worker.call(&stage.func, stage.arg, stage.arg2)
            .map_err(|e| anyhow!("[{}] stage {} ({}): {}", node_id, i, stage.func, e))?;
        println!("    stage {} ({}) done", i, stage.func);
    }

    worker.finish()?;
    println!("  PyGrouping done");
    Ok(())
}
