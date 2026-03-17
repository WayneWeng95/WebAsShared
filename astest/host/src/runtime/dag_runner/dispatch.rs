use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wasmtime::*;
use crate::policy::{EqualSlice, FixedMapPartition, FixedSizeSlice, LineBoundarySlice, ModuloPartition, RoundRobinPartition};
use crate::routing::aggregate::AggregateConnection;
use crate::routing::broadcast::BroadcastConnection;
use crate::routing::dispatch::{FileDispatcher, OwnedSlice};
use crate::routing::shuffle::ShuffleConnection;
use crate::routing::stream::StreamBridge;
use crate::runtime::input_output::slot_loader::{SlotLoader, PrefetchHandle};
use crate::runtime::input_output::logger::HostLogger;
use crate::runtime::input_output::slot_flusher::SlotFlusher;
use crate::runtime::mem_operation::reclaimer::{self, SlotKind};
use crate::runtime::mem_operation::slicer::Slicer;
use crate::runtime::worker::WorkerState;
use crate::runtime::input_output::persistence::{PersistenceOptions, PersistenceWriter};
use crate::runtime::remote::{execute_remote_recv, execute_remote_send};
use super::types::*;
use super::workers::{spawn_wasm_subprocess, spawn_python_subprocess};
use super::grouping::{execute_wasm_grouping, execute_py_grouping};
use super::pipeline::{execute_stream_pipeline, execute_py_pipeline};

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
    mesh: Option<&mut connect::MeshNode>,
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

        // ── Host routing: StreamBridge 1→1 bridge ──────────────────────────────
        NodeKind::Bridge(p) => {
            let ok = StreamBridge::new(splice_addr).bridge(p.from, p.to);
            let status = if ok { "ok" } else { "fail (source slot empty)" };
            println!("  StreamBridge::bridge({} → {}): {}", p.from, p.to, status);
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
        // Execution logic lives in pipeline.rs.
        NodeKind::StreamPipeline(p) => {
            log(&format!("stream pipeline {} rounds {} stages", p.rounds, p.stages.len()));
            execute_stream_pipeline(p, &node.id, shm_path, wasm_path)?;
            log("stream pipeline done");
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
            let outputer = SlotFlusher::new(splice_addr);
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
                reclaimer::reset_slot_cursor(splice_addr, SlotKind::Stream, s);
                println!("  FreeSlots: stream slot {} freed", s);
                log(&format!("freed stream slot {}", s));
            }
            for &s in &p.io {
                reclaimer::free_io_slot(splice_addr, s);
                reclaimer::reset_slot_cursor(splice_addr, SlotKind::Io, s);
                println!("  FreeSlots: I/O slot {} freed", s);
                log(&format!("freed I/O slot {}", s));
            }
        }

        // ── FileDispatch: load file → slice → parallel workers ───────────────
        NodeKind::FileDispatch(p) => {
            use crate::runtime::input_output::slot_loader::mmap_file;
            let loaded = mmap_file(Path::new(&p.path))
                .map_err(|e| anyhow!("[{}] mmap_file '{}': {}", node.id, p.path, e))?;
            let slicer = Slicer::new(&loaded);
            let slices = match &p.policy {
                FileDispatchPolicy::Equal        => slicer.slice(&EqualSlice,       p.workers),
                FileDispatchPolicy::LineBoundary => slicer.slice(&LineBoundarySlice, p.workers),
                FileDispatchPolicy::FixedSize { max_bytes } =>
                    slicer.slice(&FixedSizeSlice { max_bytes: *max_bytes }, p.workers),
            };
            let policy_name = match &p.policy {
                FileDispatchPolicy::Equal        => "Equal",
                FileDispatchPolicy::LineBoundary => "LineBoundary",
                FileDispatchPolicy::FixedSize{..} => "FixedSize",
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
                ShufflePolicy::Modulo => {
                    ShuffleConnection::new(&p.upstream, &p.downstream, ModuloPartition)
                        .bridge(splice_addr);
                    "Modulo"
                }
                ShufflePolicy::RoundRobin => {
                    ShuffleConnection::new(&p.upstream, &p.downstream, RoundRobinPartition::new())
                        .bridge(splice_addr);
                    "RoundRobin"
                }
                ShufflePolicy::FixedMap { map, default_slot } => {
                    let hmap: HashMap<usize, usize> =
                        map.iter().map(|&[k, v]| (k, v)).collect();
                    ShuffleConnection::new(
                        &p.upstream,
                        &p.downstream,
                        FixedMapPartition::new(hmap, *default_slot),
                    ).bridge(splice_addr);
                    "FixedMap"
                }
                ShufflePolicy::Broadcast => {
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
                let paths: &[String] = if !p.paths.is_empty() { &p.paths } else { std::slice::from_ref(&p.path) };
                let inputer = SlotLoader::new(splice_addr);
                if p.cycle && paths.len() > 1 {
                    // cycle: true — load exactly one path per run, rotating through the list.
                    let path = &paths[run_index % paths.len()];
                    inputer.load_as_single_record(Path::new(path), slot)
                        .map_err(|e| anyhow!("[{}] binary input load failed: {}", node.id, e))?;
                    println!("  Input ← \"{}\" slot {} [binary, cycle {}/{}]", path, slot, run_index % paths.len(), paths.len());
                    log(&format!("binary input (cycle) loaded '{}' → slot {}", path, slot));
                } else {
                    // cycle: false (default) — load every path as one binary record per run.
                    // Guests consume them one-at-a-time via read_next_io_record.
                    for path in paths {
                        inputer.load_as_single_record(Path::new(path), slot)
                            .map_err(|e| anyhow!("[{}] binary input load failed: {}", node.id, e))?;
                        println!("  Input ← \"{}\" slot {} [binary]", path, slot);
                        log(&format!("binary input loaded '{}' → slot {}", path, slot));
                    }
                }
            } else if p.prefetch {
                let path = if !p.paths.is_empty() {
                    p.paths[run_index % p.paths.len()].as_str()
                } else {
                    p.path.as_str()
                };
                // Fire off the load in a background thread; the executor will
                // join this handle before the first node that lists us as a dep.
                let handle = SlotLoader::prefetch(splice_addr, PathBuf::from(path), slot);
                prefetch_handles.insert(node.id.clone(), handle);
                println!("  Input ← \"{}\" slot {} [prefetch started]", path, slot);
                log(&format!("input prefetch started: '{}' → slot {}", path, slot));
            } else {
                let path = if !p.paths.is_empty() {
                    p.paths[run_index % p.paths.len()].as_str()
                } else {
                    p.path.as_str()
                };
                let count = SlotLoader::new(splice_addr)
                    .load(Path::new(path), slot)
                    .map_err(|e| anyhow!("[{}] input load failed: {}", node.id, e))?;
                println!("  Input ← \"{}\" slot {} ({} records)", path, slot, count);
                log(&format!("input loaded '{}' → slot {} ({} records)", path, slot, count));
            }
        }

        // ── PyFunc: spawn Python runner subprocess ────────────────────────────
        // NOTE: PyFunc is classified as a one-shot node (is_oneshot_node),
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

        // ── WasmGrouping: sequential WASM stages, one persistent worker each ─
        //
        // One `wasm-loop` subprocess per stage, all called in strict order.
        // No round-based overlap — this is a grouping, not a pipeline.
        // Execution logic lives in grouping.rs.
        NodeKind::WasmGrouping(p) => {
            log(&format!("wasm grouping {} stages", p.stages.len()));
            execute_wasm_grouping(p, &node.id, shm_path, wasm_path)?;
            log("wasm grouping done");
        }

        // ── PyGrouping: sequential Python stages, one shared persistent worker
        //
        // One `runner.py --loop` process for all stages, called in order.
        // No round-based overlap — this is a grouping, not a pipeline.
        // Execution logic lives in grouping.rs.
        NodeKind::PyGrouping(p) => {
            log(&format!("py grouping {} stages", p.stages.len()));
            execute_py_grouping(p, &node.id, shm_path, python_script, python_wasm)?;
            log("py grouping done");
        }

        // ── PyPipeline: true pipelined Python execution across rounds ─────────
        // Execution logic lives in pipeline.rs.
        NodeKind::PyPipeline(p) => {
            log(&format!("py pipeline {} rounds {} stages", p.rounds, p.stages.len()));
            execute_py_pipeline(p, &node.id, shm_path, python_script, python_wasm)?;
            log("py pipeline done");
        }

        // ── RemoteSend: send SHM slot records to a mesh peer ─────────────────
        NodeKind::RemoteSend(p) => {
            let mesh = mesh.ok_or_else(|| anyhow!(
                "[{}] RemoteSend requires dag.rdma to be configured", node.id
            ))?;
            log(&format!("remote send slot {} ({:?}) → peer {}", p.slot, p.slot_kind, p.peer));
            execute_remote_send(store.data().splice_addr, p.slot, p.slot_kind, p.peer, mesh)?;
            log(&format!("remote send slot {} done", p.slot));
        }

        // ── RemoteRecv: receive SHM slot records from a mesh peer ─────────────
        NodeKind::RemoteRecv(p) => {
            let mesh = mesh.ok_or_else(|| anyhow!(
                "[{}] RemoteRecv requires dag.rdma to be configured", node.id
            ))?;
            log(&format!("remote recv slot {} ({:?}) ← peer {}", p.slot, p.slot_kind, p.peer));
            execute_remote_recv(store.data().splice_addr, p.slot, p.slot_kind, p.peer, mesh)?;
            log(&format!("remote recv slot {} done", p.slot));
        }
    }

    Ok(())
}
