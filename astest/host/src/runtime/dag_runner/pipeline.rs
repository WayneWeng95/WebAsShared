use anyhow::{anyhow, Result};
use super::types::{StreamPipelineParams, PyPipelineParams};
use super::workers::{WasmLoopWorker, PyLoopWorker};

// ─── StreamPipeline executor ──────────────────────────────────────────────────

/// Execute a `StreamPipeline` node.
///
/// One persistent `wasm-loop` subprocess per stage.  Stages run in an
/// overlapping wave schedule: stage S is active on tick T when
/// `S ≤ T < rounds + S`.  Within a tick all active stages run concurrently
/// via scatter/gather (commands sent first, responses collected after), so
/// each stage's output for round R is ready before stage S+1 consumes it in
/// round R.
///
/// ```text
///   tick 0:  [stage·0·r0]
///   tick 1:  [stage·0·r1]  [stage·1·r0]
///   tick 2:  [stage·0·r2]  [stage·1·r1]  [stage·2·r0]
///   ...
/// ```
///
/// Total ticks = `rounds + depth − 1`.
pub(super) fn execute_stream_pipeline(
    params: &StreamPipelineParams,
    node_id: &str,
    shm_path: &str,
    wasm_path: &str,
) -> Result<()> {
    let rounds = params.rounds as usize;
    let depth  = params.stages.len();
    if depth == 0 {
        return Err(anyhow!("[{}] StreamPipeline has no stages", node_id));
    }

    // Spawn one persistent worker per stage (wasmtime init paid once).
    let mut workers: Vec<WasmLoopWorker> = params.stages.iter()
        .map(|s| WasmLoopWorker::spawn(&s.func, shm_path, wasm_path, node_id))
        .collect::<Result<Vec<_>>>()?;

    let slot_chain: Vec<String> = params.stages.iter()
        .map(|s| s.arg1.map_or_else(|| format!("{}(r)", s.arg0), |a| format!("{}→{}", s.arg0, a)))
        .collect();
    println!(
        "  StreamPipeline: {} rounds, {} stages | {} ({} persistent workers)",
        rounds, depth, slot_chain.join(" › "), depth
    );

    let total_ticks = rounds + depth - 1;
    for tick in 0..total_ticks {
        // Collect active (stage_idx, resolved_arg1) pairs for this tick.
        let active: Vec<(usize, u32)> = params.stages.iter().enumerate()
            .filter_map(|(s_idx, stage)| {
                let tick_round = tick as isize - s_idx as isize;
                if tick_round >= 0 && (tick_round as usize) < rounds {
                    Some((s_idx, stage.arg1.unwrap_or(tick_round as u32)))
                } else {
                    None
                }
            })
            .collect();

        // Scatter: write all commands first — workers wake and run concurrently.
        for &(s_idx, a1) in &active {
            workers[s_idx].send(params.stages[s_idx].arg0, a1)
                .map_err(|e| anyhow!("[{}] stage {} tick {} send: {}", node_id, s_idx, tick, e))?;
        }
        // Gather: collect responses in order (workers may already be done).
        for &(s_idx, _) in &active {
            workers[s_idx].recv()
                .map_err(|e| anyhow!("[{}] stage {} tick {}: {}", node_id, s_idx, tick, e))?;
        }

        println!("    tick {} complete", tick);
    }

    // Close stdin on all workers (EOF → they exit) and wait for them.
    for w in workers {
        w.finish()?;
    }

    // Dump the accumulated per-round summaries from the last stage's output slot.
    if let Some(summary_slot) = params.stages.last().and_then(|s| s.arg1) {
        let exe = std::env::current_exe()
            .map_err(|e| anyhow!("cannot find current exe: {}", e))?;
        let status = std::process::Command::new(exe)
            .args(["wasm-call", shm_path, wasm_path, "dump_stream_records", "fatptr",
                   &summary_slot.to_string()])
            .status()
            .map_err(|e| anyhow!("[{}] dump_stream_records spawn: {}", node_id, e))?;
        if !status.success() {
            return Err(anyhow!("[{}] dump_stream_records failed", node_id));
        }
    }

    println!("  StreamPipeline done");
    Ok(())
}

// ─── PyPipeline executor ──────────────────────────────────────────────────────

/// Execute a `PyPipeline` node.
///
/// One persistent `runner.py --loop` process per stage.  Mirrors
/// `StreamPipeline`'s wave schedule but for Python workloads: stage S is
/// active on tick T when `S ≤ T < rounds + S`.  Within a tick all active
/// stages run concurrently via scatter/gather:
///
/// ```text
///   tick 0:  [stage·0·r0]
///   tick 1:  [stage·0·r1]  [stage·1·r0]
///   tick 2:  [stage·0·r2]  [stage·1·r1]  [stage·2·r0]
///   ...
/// ```
///
/// When a stage's `arg2` is `None`, the current round index is injected so
/// the worker can use it as an output slot selector or cursor hint.
///
/// Total ticks = `rounds + depth − 1`.
pub(super) fn execute_py_pipeline(
    params: &PyPipelineParams,
    node_id: &str,
    shm_path: &str,
    python_script: &str,
    python_wasm: Option<&str>,
) -> Result<()> {
    let rounds = params.rounds as usize;
    let depth  = params.stages.len();
    if depth == 0 {
        return Err(anyhow!("[{}] PyPipeline has no stages", node_id));
    }

    // Spawn one persistent Python worker per stage (startup paid once).
    let mut workers: Vec<PyLoopWorker> = (0..depth)
        .map(|_| PyLoopWorker::spawn(shm_path, python_script, python_wasm, node_id))
        .collect::<Result<Vec<_>>>()?;

    let slot_chain: Vec<String> = params.stages.iter()
        .map(|s| s.arg2.map_or_else(|| format!("{}(r)", s.arg), |a2| format!("{}→{}", s.arg, a2)))
        .collect();
    println!(
        "  PyPipeline: {} rounds, {} stages | {} ({} persistent workers)",
        rounds, depth, slot_chain.join(" › "), depth
    );

    let total_ticks = rounds + depth - 1;
    for tick in 0..total_ticks {
        // Collect active (stage_idx, resolved_arg2) pairs for this tick.
        let active: Vec<(usize, Option<u32>)> = params.stages.iter().enumerate()
            .filter_map(|(s_idx, stage)| {
                let tick_round = tick as isize - s_idx as isize;
                if tick_round >= 0 && (tick_round as usize) < rounds {
                    let resolved = stage.arg2.or(Some(tick_round as u32));
                    Some((s_idx, resolved))
                } else {
                    None
                }
            })
            .collect();

        // Scatter: write all commands first — workers wake and run concurrently.
        for &(s_idx, a2) in &active {
            workers[s_idx].call_async(&params.stages[s_idx].func, params.stages[s_idx].arg, a2)
                .map_err(|e| anyhow!("[{}] stage {} tick {} send: {}", node_id, s_idx, tick, e))?;
        }
        // Gather: collect responses in order.
        for &(s_idx, _) in &active {
            workers[s_idx].recv()
                .map_err(|e| anyhow!("[{}] stage {} tick {}: {}", node_id, s_idx, tick, e))?;
        }

        println!("    tick {} complete", tick);
    }

    // Close stdin on all workers (EOF → they exit) and wait.
    for w in workers {
        w.finish()?;
    }

    println!("  PyPipeline done");
    Ok(())
}
