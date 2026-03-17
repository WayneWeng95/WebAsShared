use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wasmtime::*;
use crate::policy::{EqualSlice, FixedMapPartition, FixedSizeSlice, LineBoundarySlice, ModuloPartition, RoundRobinPartition};
use crate::routing::aggregate::AggregateConnection;
use crate::routing::broadcast::BroadcastConnection;
use crate::routing::dispatch::{FileDispatcher, OwnedSlice};
use crate::routing::shuffle::ShuffleConnection;
use crate::routing::stream::HostStream;
use crate::runtime::input_output::inputer::{Inputer, PrefetchHandle};
use crate::runtime::input_output::logger::HostLogger;
use crate::runtime::input_output::outputer::Outputer;
use crate::runtime::mem_operation::reclaimer;
use crate::runtime::mem_operation::slicer::Slicer;
use crate::runtime::worker::WorkerState;
use crate::runtime::input_output::state_writer::{PersistenceOptions, PersistenceWriter};
use super::types::*;
use super::subprocess::{spawn_wasm_subprocess, spawn_python_subprocess, PipelineWorker, PyPipelineWorker};

// ─── Node executor ────────────────────────────────────────────────────────────

pub(super) fn execute_node(
    node: &DagNode,
    store: &mut Store<WorkerState>,
    instance: &Instance,
    memory: &Memory,
    persist_writer: Option<&PersistenceWriter>,
    logger: Option<&HostLogger>,
    prefetch_handles: &mut HashMap<String, PrefetchHandle>,
    run_index: usize,
    shm_path: &str,
    python_script: &str,
    python_wasm: Option<&str>,
    wasm_path: &str,
) -> Result<()> {
    let splice_addr = store.data().splice_addr;
    let base_ptr = memory.data_ptr(&*store);

    // Shorthand: log at info level tagged with the node id.
    let log = |msg: &str| {
        if let Some(lg) = logger {
            lg.info(&node.id, msg);
        }
    };
    let log_debug = |msg: &str| {
        if let Some(lg) = logger {
            lg.debug(&node.id, msg);
        }
    };

    match &node.kind {
        // ── WASM subprocess nodes — spawned as isolated child processes ───────
        NodeKind::WasmVoid(_) | NodeKind::WasmU32(_) | NodeKind::WasmFatPtr(_) => {
            log_debug(&format!("spawn subprocess for node {}", node.id));
            let mut child = spawn_wasm_subprocess(node, shm_path, wasm_path)?;
            let status = child.wait()
                .map_err(|e| anyhow!("[{}] failed to wait for WASM worker: {}", node.id, e))?;
            if !status.success() {
                return Err(anyhow!("[{}] WASM worker exited with {}", node.id, status));
            }
            log(&format!("node {} done", node.id));
        }

        // ── Host routing: HostStream 1→1 bridge ──────────────────────────────
        NodeKind::Bridge(p) => {
            let ok = HostStream::new(splice_addr).bridge(p.from, p.to);
            let status = if ok { "ok" } else { "fail (source slot empty)" };
            println!("  HostStream::bridge({} → {}): {}", p.from, p.to, status);
            log(&format!("bridge {} → {}: {}", p.from, p.to, status));
        }

        // ── Host routing: AggregateConnection N→1 ────────────────────────────
        NodeKind::Aggregate(p) => {
            log(&format!("aggregate {:?} → {}", p.upstream, p.downstream));
            AggregateConnection::new(&p.upstream, p.downstream).bridge(splice_addr);
            println!("  AggregateConnection({:?} → {}): done", p.upstream, p.downstream);
            log(&format!("aggregate {:?} → {} done", p.upstream, p.downstream));
        }

        // ── Lightweight single-item watch ─────────────────────────────────────
        NodeKind::Watch(p) => {
            match persist_writer {
                None => {
                    println!("  Watch: no writer available — skipped");
                    log("watch skipped: no persistence writer");
                }
                Some(w) => match (&p.stream, &p.shared) {
                    (Some(slot), None) => {
                        let slot = *slot;
                        w.watch_stream(splice_addr, slot, &p.output);
                        println!("  Watch stream {} → \"{}\" [background]", slot, p.output);
                        log(&format!("watch stream {} → \"{}\"", slot, p.output));
                    }
                    (None, Some(name)) => {
                        w.watch_shared(splice_addr, name, &p.output);
                        println!("  Watch shared \"{}\" → \"{}\" [background]", name, p.output);
                        log(&format!("watch shared \"{}\" → \"{}\"", name, p.output));
                    }
                    _ => println!("  Watch: set exactly one of `stream` or `shared`"),
                },
            }
        }

        // ── Background persistence snapshot ───────────────────────────────────
        NodeKind::Persist(p) => {
            let opts = PersistenceOptions {
                output_dir:   p.output_dir.clone(),
                atomics:      p.atomics,
                stream_slots: p.stream_slots.clone(),
                shared_state: p.shared_state,
            };
            match persist_writer {
                Some(w) => {
                    w.snapshot(splice_addr, &opts);
                    println!(
                        "  Persist(atomics={}, streams={:?}, shared={}) → \"{}\" [background]",
                        p.atomics, p.stream_slots, p.shared_state, p.output_dir
                    );
                    log(&format!(
                        "persist atomics={} streams={:?} shared={} → \"{}\"",
                        p.atomics, p.stream_slots, p.shared_state, p.output_dir
                    ));
                }
                None => {
                    println!("  Persist: no writer available — skipped");
                    log("persist skipped: no persistence writer");
                }
            }
        }

        // ── Streaming pipeline: pipelined execution over DAG-defined stages ──
        //
        // Stage S is active on tick T when S ≤ T < rounds + S.
        // Active stages within a tick are independent; the tick barrier ensures
        // each stage only sees data committed by the previous stage in the prior tick.
        //
        //   tick 0:  [stage·0·r0]
        //   tick 1:  [stage·0·r1]  [stage·1·r0]
        //   tick 2:  [stage·0·r2]  [stage·1·r1]  [stage·2·r0]
        //   ...
        //
        // Total ticks = rounds + depth − 1.
        //
        // Workers are persistent `wasm-loop` subprocesses: one per stage, spawned
        // once before the tick loop and reused across all rounds.  Each worker
        // sleeps (blocked on stdin read) between ticks; the host wakes it by
        // writing a command line.  Active stages within a tick run concurrently
        // via a scatter/gather pattern: all commands are sent first so workers
        // execute in parallel, then responses are collected in order.
        NodeKind::StreamPipeline(p) => {
            let rounds = p.rounds as usize;
            let depth  = p.stages.len();
            if depth == 0 {
                return Err(anyhow!("[{}] StreamPipeline has no stages", node.id));
            }

            // Spawn one persistent worker per stage (wasmtime init paid once).
            let mut workers: Vec<PipelineWorker> = p.stages.iter()
                .map(|s| PipelineWorker::spawn(&s.func, shm_path, wasm_path, &node.id))
                .collect::<Result<Vec<_>>>()?;

            let slot_chain: Vec<String> = p.stages.iter()
                .map(|s| s.arg1.map_or_else(|| format!("{}(r)", s.arg0), |a| format!("{}→{}", s.arg0, a)))
                .collect();
            println!("  StreamPipeline: {} rounds, {} stages | {} ({} persistent workers)",
                rounds, depth, slot_chain.join(" › "), depth);
            log(&format!("pipeline {} rounds {} stages", rounds, depth));

            let total_ticks = rounds + depth - 1;
            for tick in 0..total_ticks {
                // Collect active (stage_idx, resolved_arg1) pairs for this tick.
                let active: Vec<(usize, u32)> = p.stages.iter().enumerate()
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
                    workers[s_idx].send(p.stages[s_idx].arg0, a1)
                        .map_err(|e| anyhow!("[{}] stage {} tick {} send: {}", node.id, s_idx, tick, e))?;
                }
                // Gather: collect responses in order (workers may already be done).
                for &(s_idx, _) in &active {
                    workers[s_idx].recv()
                        .map_err(|e| anyhow!("[{}] stage {} tick {}: {}", node.id, s_idx, tick, e))?;
                }

                println!("    tick {} complete", tick);
                log_debug(&format!("tick {} complete", tick));
            }

            // Close stdin on all workers (EOF → they exit) and wait for them.
            for w in workers {
                w.finish()?;
            }

            // Dump the accumulated per-round summaries from the last stage's output slot.
            if let Some(summary_slot) = p.stages.last().and_then(|s| s.arg1) {
                let exe = std::env::current_exe()
                    .map_err(|e| anyhow!("cannot find current exe: {}", e))?;
                let status = std::process::Command::new(exe)
                    .args(["wasm-call", shm_path, wasm_path, "dump_stream_records", "fatptr",
                           &summary_slot.to_string()])
                    .status()
                    .map_err(|e| anyhow!("[{}] dump_stream_records spawn: {}", node.id, e))?;
                if !status.success() {
                    return Err(anyhow!("[{}] dump_stream_records failed", node.id));
                }
            }
        }

        // ── Output: flush reserved output slot to a file ─────────────────────
        NodeKind::Output(p) => {
            use common::OUTPUT_IO_SLOT;
            let slot = p.slot.unwrap_or(OUTPUT_IO_SLOT);
            let path = if !p.paths.is_empty() {
                p.paths[run_index % p.paths.len()].as_str()
            } else {
                p.path.as_str()
            };
            let outputer = Outputer::new(splice_addr);
            let count = outputer.save_slot(Path::new(path), slot)
                .map_err(|e| anyhow!("[{}] output save failed: {}", node.id, e))?;
            println!("  Output slot {} → \"{}\" ({} records)", slot, path, count);
            log(&format!("output slot {} saved to \"{}\" ({} records)", slot, path, count));
        }

        // ── FreeSlots: explicit slot reset between sequential pipeline runs ──
        NodeKind::FreeSlots(p) => {
            let splice_addr = store.data().splice_addr;
            for &s in &p.stream {
                reclaimer::free_stream_slot(splice_addr, s);
                println!("  FreeSlots: stream slot {} freed", s);
                log(&format!("freed stream slot {}", s));
            }
            for &s in &p.io {
                reclaimer::free_io_slot(splice_addr, s);
                println!("  FreeSlots: I/O slot {} freed", s);
                log(&format!("freed I/O slot {}", s));
            }
        }

        // ── FileDispatch: load file → slice → parallel workers ───────────────
        NodeKind::FileDispatch(p) => {
            use crate::runtime::input_output::inputer::load_file;
            let loaded = load_file(Path::new(&p.path))
                .map_err(|e| anyhow!("[{}] load_file '{}': {}", node.id, p.path, e))?;
            let slicer = Slicer::new(&loaded);
            let slices = match &p.policy {
                SlicePolicySpec::Equal        => slicer.slice(&EqualSlice,       p.workers),
                SlicePolicySpec::LineBoundary => slicer.slice(&LineBoundarySlice, p.workers),
                SlicePolicySpec::FixedSize { max_bytes } =>
                    slicer.slice(&FixedSizeSlice { max_bytes: *max_bytes }, p.workers),
            };
            let policy_name = match &p.policy {
                SlicePolicySpec::Equal        => "Equal",
                SlicePolicySpec::LineBoundary => "LineBoundary",
                SlicePolicySpec::FixedSize{..} => "FixedSize",
            };
            println!(
                "  FileDispatch: '{}' ({} bytes) → {} slices, {} workers, policy={}",
                p.path, loaded.len(), slices.len(), p.workers, policy_name
            );
            log(&format!(
                "dispatch '{}' {} bytes {} slices {} workers policy={}",
                p.path, loaded.len(), slices.len(), p.workers, policy_name
            ));
            FileDispatcher::new(p.workers).run(slices, |assignment| {
                println!(
                    "    [FileDispatch] worker {} → {} slices, {} bytes",
                    assignment.worker_id, assignment.slice_count(), assignment.total_bytes()
                );
            });
            println!("  FileDispatch done");
            log("file dispatch done");
        }

        // ── OwnedDispatch: inline payloads → parallel workers ─────────────────
        NodeKind::OwnedDispatch(p) => {
            let slices: Vec<OwnedSlice> = p.items
                .iter()
                .enumerate()
                .map(|(i, s)| OwnedSlice { index: i, data: s.as_bytes().to_vec() })
                .collect();
            let total: usize = slices.iter().map(|s| s.data.len()).sum();
            println!(
                "  OwnedDispatch: {} items ({} bytes total) → {} workers",
                slices.len(), total, p.workers
            );
            log(&format!(
                "owned dispatch {} items {} bytes {} workers",
                slices.len(), total, p.workers
            ));
            FileDispatcher::new(p.workers).run(slices, |assignment| {
                println!(
                    "    [OwnedDispatch] worker {} → {} slices, {} bytes",
                    assignment.worker_id, assignment.slice_count(), assignment.total_bytes()
                );
                for s in &assignment.slices {
                    let text = std::str::from_utf8(&s.data).unwrap_or("<binary>");
                    println!("      slice[{}]: {:?}", s.index, text);
                }
            });
            println!("  OwnedDispatch done");
            log("owned dispatch done");
        }

        // ── Host routing: ShuffleConnection N→M ──────────────────────────────
        NodeKind::Shuffle(p) => {
            let policy_name = match &p.policy {
                PolicySpec::Modulo => {
                    ShuffleConnection::new(&p.upstream, &p.downstream, ModuloPartition)
                        .bridge(splice_addr);
                    "Modulo"
                }
                PolicySpec::RoundRobin => {
                    ShuffleConnection::new(&p.upstream, &p.downstream, RoundRobinPartition::new())
                        .bridge(splice_addr);
                    "RoundRobin"
                }
                PolicySpec::FixedMap { map, default_slot } => {
                    let hmap: HashMap<usize, usize> =
                        map.iter().map(|&[k, v]| (k, v)).collect();
                    ShuffleConnection::new(
                        &p.upstream,
                        &p.downstream,
                        FixedMapPartition::new(hmap, *default_slot),
                    ).bridge(splice_addr);
                    "FixedMap"
                }
                PolicySpec::Broadcast => {
                    BroadcastConnection::new(&p.upstream, &p.downstream)
                        .bridge(splice_addr);
                    "Broadcast"
                }
            };
            println!(
                "  ShuffleConnection({:?} → {:?}, {}): done",
                p.upstream, p.downstream, policy_name
            );
            log(&format!(
                "shuffle {:?} → {:?} policy={} done",
                p.upstream, p.downstream, policy_name
            ));
        }

        // ── Input: load file → reserved input slot ────────────────────────────
        NodeKind::Input(p) => {
            use common::INPUT_IO_SLOT;
            let slot = p.slot.unwrap_or(INPUT_IO_SLOT);
            if p.binary {
                // Load every path as a single binary record each run.
                // Guests consume them one-at-a-time via read_next_io_record.
                let paths: &[String] = if !p.paths.is_empty() { &p.paths } else { std::slice::from_ref(&p.path) };
                let inputer = Inputer::new(splice_addr);
                for path in paths {
                    inputer.load_as_single_record(Path::new(path), slot)
                        .map_err(|e| anyhow!("[{}] binary input load failed: {}", node.id, e))?;
                    println!("  Input ← \"{}\" slot {} [binary]", path, slot);
                    log(&format!("binary input loaded '{}' → slot {}", path, slot));
                }
            } else if p.prefetch {
                let path = if !p.paths.is_empty() {
                    p.paths[run_index % p.paths.len()].as_str()
                } else {
                    p.path.as_str()
                };
                // Fire off the load in a background thread; the executor will
                // join this handle before the first node that lists us as a dep.
                let handle = Inputer::prefetch(splice_addr, PathBuf::from(path), slot);
                prefetch_handles.insert(node.id.clone(), handle);
                println!("  Input ← \"{}\" slot {} [prefetch started]", path, slot);
                log(&format!("input prefetch started: '{}' → slot {}", path, slot));
            } else {
                let path = if !p.paths.is_empty() {
                    p.paths[run_index % p.paths.len()].as_str()
                } else {
                    p.path.as_str()
                };
                let count = Inputer::new(splice_addr)
                    .load(Path::new(path), slot)
                    .map_err(|e| anyhow!("[{}] input load failed: {}", node.id, e))?;
                println!("  Input ← \"{}\" slot {} ({} records)", path, slot, count);
                log(&format!("input loaded '{}' → slot {} ({} records)", path, slot, count));
            }
        }

        // ── PyFunc: spawn Python runner subprocess ────────────────────────────
        // NOTE: PyFunc is classified as a subprocess node (is_subprocess_node),
        // so in normal wave execution it is spawned in parallel via
        // spawn_python_subprocess and never reaches execute_node.  This branch
        // is retained as a sequential fallback (spawn + wait) for direct calls.
        NodeKind::PyFunc(_) => {
            log_debug(&format!("PyFunc subprocess for node {}", node.id));
            let mut child = spawn_python_subprocess(node, shm_path, python_script, python_wasm)?;
            let status = child.wait()
                .map_err(|e| anyhow!("[{}] failed to wait for Python worker: {}", node.id, e))?;
            if !status.success() {
                return Err(anyhow!("[{}] Python worker exited with {}", node.id, status));
            }
            log(&format!("node {} done", node.id));
        }

        // ── PyPipeline: persistent Python worker for sequential stages ────────
        //
        // One `runner.py --loop` process is spawned for the full pipeline.
        // Each stage sends "func arg [arg2]\n" to its stdin and waits for
        // "ok\n".  The process sleeps between calls (blocked on stdin read),
        // so no CPU is consumed between stages.  Closing stdin exits the worker.
        NodeKind::PyPipeline(p) => {
            if p.stages.is_empty() {
                return Err(anyhow!("[{}] PyPipeline has no stages", node.id));
            }
            let mut worker = PyPipelineWorker::spawn(shm_path, python_script, python_wasm, &node.id)?;
            println!("  PyPipeline: {} stages, 1 persistent worker", p.stages.len());
            log(&format!("pypipeline {} stages", p.stages.len()));

            for (i, stage) in p.stages.iter().enumerate() {
                worker.call(&stage.func, stage.arg, stage.arg2)
                    .map_err(|e| anyhow!("[{}] stage {} ({}): {}", node.id, i, stage.func, e))?;
                log_debug(&format!("stage {} ({}) done", i, stage.func));
            }

            worker.finish()?;
            println!("  PyPipeline done");
            log("pypipeline done");
        }
    }

    Ok(())
}
