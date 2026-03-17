//! DAG-based execution engine for stream routing scenarios.
//!
//! Reads a JSON DAG description, topologically sorts the nodes (Kahn's algorithm),
//! then runs each node in order against a single shared WASM instance backed by
//! a freshly-formatted SHM region.
//!
//! # JSON format
//! ```json
//! {
//!   "shm_path": "/dev/shm/my_dag_shm",
//!   "nodes": [
//!     { "id": "p0", "deps": [],            "kind": { "WasmVoid":   { "func": "produce_stream",      "arg": 0 } } },
//!     { "id": "p1", "deps": [],            "kind": { "WasmVoid":   { "func": "produce_stream",      "arg": 1 } } },
//!     { "id": "route", "deps": ["p0","p1"],"kind": { "Shuffle":    { "upstream":[0,1], "downstream":[2,3], "policy":{"type":"Modulo"} } } },
//!     { "id": "r2", "deps": ["route"],     "kind": { "WasmFatPtr": { "func": "dump_stream_records", "arg": 2 } } },
//!     { "id": "r3", "deps": ["route"],     "kind": { "WasmFatPtr": { "func": "dump_stream_records", "arg": 3 } } }
//!   ]
//! }
//! ```
//!
//! ## Node kinds
//! - `WasmVoid`   — calls `func(arg: u32) -> ()`
//! - `WasmU32`    — calls `func(arg: u32) -> u32`, prints the result
//! - `WasmFatPtr` — calls `func(arg: u32) -> u64`, decodes fat-pointer and prints all records
//! - `Bridge`     — `HostStream::bridge(from, to)` (zero-copy 1→1 wire)
//! - `Aggregate`  — `AggregateConnection::new(upstream, downstream).bridge()`
//! - `Shuffle`    — `ShuffleConnection::new(upstream, downstream, policy).bridge()`
//! - `Persist`    — snapshot SHM data to storage in a background thread
//! - `Watch`      — lightweight: persist one stream slot or one shared-state entry
//! - `Input`      — load a file into a slot; guest reads via `ShmApi::read_all_inputs_from(slot)`
//! - `Output`     — flush a slot to a file; guest wrote via `ShmApi::write_output_to(slot, data)`
//!
//! ## Input node
//! ```json
//! { "id": "load", "deps": [], "kind": { "Input": { "path": "/data/rows.csv" } } }
//! { "id": "load", "deps": [], "kind": { "Input": { "path": "/data/rows.csv", "slot": 42 } } }
//! { "id": "load", "deps": [], "kind": { "Input": { "path": "/data/rows.csv", "slot": 42, "prefetch": true } } }
//! ```
//! Omitting `"slot"` defaults to `INPUT_IO_SLOT`.
//! With `"prefetch": true` the I/O runs in a background thread, overlapping
//! with any independent nodes that run before the first node that depends on
//! this `Input` node.
//!
//! ## Output node
//! ```json
//! { "id": "save", "deps": ["worker"], "kind": { "Output": { "path": "/tmp/result.txt" } } }
//! { "id": "save", "deps": ["worker"], "kind": { "Output": { "path": "/tmp/result.txt", "slot": 42 } } }
//! ```
//! Omitting `"slot"` defaults to `OUTPUT_IO_SLOT`.
//!
//! ## Watch node (lightweight)
//! ```json
//! { "kind": { "Watch": { "stream": 50, "output": "/tmp/out/slot50.txt" } } }
//! { "kind": { "Watch": { "shared": "FuncA_Result", "output": "/tmp/out/funcA.bin" } } }
//! ```
//!
//! ## Persist node
//! ```json
//! {
//!   "id": "save", "deps": ["some_node"],
//!   "kind": {
//!     "Persist": {
//!       "output_dir": "/tmp/dag_out",
//!       "atomics": true,
//!       "stream_slots": [50, 51],
//!       "shared_state": true
//!     }
//!   }
//! }
//! ```
//!
//! ## Shuffle policies
//! ```json
//! { "type": "Modulo" }
//! { "type": "RoundRobin" }
//! { "type": "FixedMap", "map": [[0,1],[1,0]], "default_slot": 0 }
//! { "type": "Broadcast" }
//! ```
//!
//! ## Execution mode
//! Set the optional `"mode"` field to control what happens after all nodes finish:
//! ```json
//! { "shm_path": "...", "mode": "one_shot", "nodes": [...] }
//! { "shm_path": "...", "mode": "reset",    "nodes": [...] }
//! ```
//! - `"one_shot"` (default) — execute once and exit.
//! - `"reset"` — re-execute immediately from the first node using the **same**
//!   WASM instance and SHM connection, looping until SIGINT (Ctrl-C).
//!
//! ## Python WASM execution
//! Set the optional `"python_wasm"` field to run `PyFunc` nodes through a
//! pre-built `python.wasm` binary via `wasmtime run` instead of the host's
//! native `python3`:
//! ```json
//! {
//!   "shm_path": "...",
//!   "python_script": "../py_guest/python/runner.py",
//!   "python_wasm": "/opt/myapp/python-3.12.0.wasm",
//!   "nodes": [...]
//! }
//! ```
//! The framework automatically mounts the SHM parent directory and the script
//! directory as WASI preopens, so the guest can read/write the SHM file and
//! import the `workloads` module.  Requires `wasmtime` to be on `PATH`.
//! Omit `"python_wasm"` (or leave it `null`) to fall back to native `python3`.
//!
//! ## Logging
//! Set the optional `"log_level"` field to enable host-side logging into the
//! SHM LOG_ARENA (readable alongside guest log output via the `func_b` reader):
//! ```json
//! { "shm_path": "...", "log_level": "info", "nodes": [...] }
//! ```
//! Accepted values (case-insensitive): `"debug"`, `"info"`, `"warn"`, `"error"`.
//! Omit the field (or set it to `"off"`) to disable logging entirely.

mod types;
mod plan;
mod subprocess;
mod executor;

pub use types::*;

use anyhow::{anyhow, Result};
use serde_json;
use std::collections::HashMap;
use std::fs::OpenOptions;
use wasmtime::*;
use crate::runtime::input_output::inputer::PrefetchHandle;
use crate::runtime::input_output::logger::HostLogger;
use crate::runtime::mem_operation::reclaimer::{self, SlotKind};
use crate::runtime::worker::{create_wasmtime_engine, setup_vma_environment, WorkerState};
use crate::runtime::input_output::state_writer::PersistenceWriter;
use crate::shm::format_shared_memory;
use common::WASM_PATH;
use plan::{build_slot_refcounts, build_waves, is_subprocess_node, node_owned_slots, node_routed_upstream_slots, parse_level, topo_sort, validate_dag};
use subprocess::{spawn_python_subprocess, spawn_wasm_subprocess};
use executor::execute_node;

// ─── Public entry points ──────────────────────────────────────────────────────

/// Load a DAG from a JSON **file** and execute it.
pub fn run_dag_file(json_path: &str) -> Result<()> {
    let json = std::fs::read_to_string(json_path)
        .map_err(|e| anyhow!("Cannot read DAG file '{}': {}", json_path, e))?;
    run_dag_json(&json)
}

/// Load a DAG from a JSON **string** and execute it.
pub fn run_dag_json(json: &str) -> Result<()> {
    let dag: Dag = serde_json::from_str(json)
        .map_err(|e| anyhow!("Invalid DAG JSON: {}", e))?;
    run_dag(&dag)
}

/// Execute a pre-parsed [`Dag`].
///
/// Behaviour depends on [`DagMode`]:
/// - `one_shot`: execute all nodes once and return.
/// - `reset`:    execute all nodes, then immediately re-execute from the
///               beginning using the **same** WASM instance and SHM
///               connection.  Loops until SIGINT (Ctrl-C).
pub fn run_dag(dag: &Dag) -> Result<()> {
    println!("[DAG] Starting — shm: {} (mode: {:?})", dag.shm_path, dag.mode);

    validate_dag(dag)?;

    // Format a fresh SHM region so prior data never leaks into the first run.
    format_shared_memory(&dag.shm_path)?;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&dag.shm_path)?;

    let engine = create_wasmtime_engine()?;
    let mut store = Store::new(
        &engine,
        WorkerState { file: file.try_clone()?, splice_addr: 0 },
    );
    let mut linker = Linker::new(&engine);
    let memory = setup_vma_environment(&mut store, &mut linker, &file)?;

    let wasm_path = dag.wasm_path.as_deref().unwrap_or(WASM_PATH);
    let module = Module::from_file(&engine, wasm_path)?;
    let instance = linker.instantiate(&mut store, &module)?;
    let py_script = dag.python_script.as_deref().unwrap_or("");
    let py_wasm   = dag.python_wasm.as_deref();

    // Build an optional logger now that splice_addr is known.
    let splice_addr = store.data().splice_addr;
    let logger: Option<HostLogger> = dag.log_level
        .as_deref()
        .and_then(parse_level)
        .map(|lvl| HostLogger::with_level(splice_addr, lvl));

    if let Some(ref lg) = logger {
        lg.info("DAG", &format!("starting — shm: {}", dag.shm_path));
    }

    // Topological order is fixed for all iterations.
    let order = topo_sort(&dag.nodes)?;
    let node_ids: Vec<&str> = order.iter().map(|&i| dag.nodes[i].id.as_str()).collect();
    println!("[DAG] Execution order: {}", node_ids.join(" → "));
    if let Some(ref lg) = logger {
        lg.info("DAG", &format!("execution order: {}", node_ids.join(" → ")));
    }

    let waves = build_waves(&dag.nodes, &order);
    println!("[DAG] {} waves, {} nodes total", waves.len(), order.len());

    let has_persistence = dag.nodes.iter().any(|n| matches!(n.kind, NodeKind::Persist(_) | NodeKind::Watch(_)));

    // Build a map from each Input node's id to (slot, number_of_direct_consumers).
    // Input I/O slots must be freed after all nodes that list the Input as a dep
    // have completed — this is computed once since the DAG structure is fixed.
    let input_dep_counts: HashMap<String, (u32, usize)> = {
        use common::INPUT_IO_SLOT;
        let input_slots: HashMap<&str, u32> = dag.nodes.iter()
            .filter_map(|n| {
                if let NodeKind::Input(p) = &n.kind {
                    Some((n.id.as_str(), p.slot.unwrap_or(INPUT_IO_SLOT)))
                } else {
                    None
                }
            })
            .collect();
        let mut counts: HashMap<String, (u32, usize)> = HashMap::new();
        for node in &dag.nodes {
            for dep_id in &node.deps {
                if let Some(&slot) = input_slots.get(dep_id.as_str()) {
                    counts.entry(dep_id.clone()).or_insert((slot, 0)).1 += 1;
                }
            }
        }
        counts
    };

    let mut run_count = 0u32;

    loop {
        run_count += 1;
        if dag.mode == DagMode::Reset && run_count > 1 {
            println!("[DAG] ══ Reset — run #{} ══", run_count);
            if let Some(ref lg) = logger {
                lg.info("DAG", &format!("reset run #{}", run_count));
            }
        }

        // Per-run state: rebuilt every iteration so slot counts are correct.
        let mut persist_writer = if has_persistence { Some(PersistenceWriter::new()) } else { None };
        let mut prefetch_handles: HashMap<String, PrefetchHandle> = HashMap::new();
        let mut slot_refcounts = build_slot_refcounts(dag);
        // Per-run countdown for Input slot reclamation (reset each iteration).
        let mut input_dep_remaining = input_dep_counts.clone();

        // Run each wave
        for wave in &waves {
            // 1. Pre-join prefetches for all deps of nodes in this wave.
            for &idx in wave {
                let node = &dag.nodes[idx];
                for dep_id in &node.deps {
                    if let Some(handle) = prefetch_handles.remove(dep_id) {
                        let slot = handle.slot;
                        let count = handle.join()
                            .map_err(|e| anyhow!("prefetch '{}' failed: {}", dep_id, e))?;
                        println!("[DAG] Prefetch '{}' ready ({} records in slot {})", dep_id, count, slot);
                    }
                }
            }

            // 2. Partition wave: subprocess nodes (WASM + PyFunc) vs host (routing + StreamPipeline).
            let (sub_idxs, host_idxs): (Vec<usize>, Vec<usize>) = wave.iter()
                .partition(|&&idx| is_subprocess_node(&dag.nodes[idx].kind));

            if wave.len() > 1 {
                println!("[DAG] Wave: {} nodes in parallel ({} subprocess + {} host)",
                    wave.len(), sub_idxs.len(), host_idxs.len());
            }

            // 3a. Spawn all subprocess nodes (WASM + PyFunc) in parallel.
            let mut children: Vec<(String, std::process::Child)> = sub_idxs
                .iter()
                .map(|&idx| {
                    let node = &dag.nodes[idx];
                    println!("[DAG] ── Node: {} ──", node.id);
                    let child = match &node.kind {
                        NodeKind::WasmVoid(_) | NodeKind::WasmU32(_) | NodeKind::WasmFatPtr(_) =>
                            spawn_wasm_subprocess(node, dag.shm_path.as_str(), wasm_path)?,
                        NodeKind::PyFunc(_) =>
                            spawn_python_subprocess(node, dag.shm_path.as_str(), py_script, py_wasm)?,
                        _ => unreachable!(),
                    };
                    Ok((node.id.clone(), child))
                })
                .collect::<Result<Vec<_>>>()?;

            // 3b. Run host nodes on main thread (concurrent with subprocess children).
            // StreamPipeline falls here and spawns its own sequential subprocesses per stage.
            for &idx in &host_idxs {
                let node = &dag.nodes[idx];
                println!("[DAG] ── Node: {} ──", node.id);
                execute_node(node, &mut store, &instance, &memory,
                             persist_writer.as_ref().map(|w| w as &PersistenceWriter),
                             logger.as_ref().map(|l| l as &HostLogger),
                             &mut prefetch_handles, (run_count - 1) as usize,
                             &dag.shm_path, py_script, py_wasm, wasm_path)?;
            }

            // 3c. Wait for all subprocess WASM children.
            for (id, mut child) in children {
                let status = child.wait()
                    .map_err(|e| anyhow!("[{}] failed to wait for WASM worker: {}", id, e))?;
                if !status.success() {
                    return Err(anyhow!("[{}] WASM worker exited with {}", id, status));
                }
                println!("  [{}] → ok", id);
            }

            // 4. Post-wave slot reclamation for all nodes in wave.
            let splice_addr = store.data().splice_addr;
            for &idx in wave {
                let node = &dag.nodes[idx];

                // Routing upstreams: page chains have been transferred into downstream
                // slots by chain_onto.  Zero only the metadata.
                for s in node_routed_upstream_slots(&node.kind) {
                    reclaimer::clear_stream_slot(splice_addr, s);
                }

                // Exclusively-owned slots: freed when the last reader finishes.
                let (owned_streams, owned_ios) = node_owned_slots(&node.kind);

                for s in owned_streams {
                    let key = (SlotKind::Stream, s);
                    if let Some(count) = slot_refcounts.get_mut(&key) {
                        *count -= 1;
                        if *count == 0 {
                            reclaimer::free_stream_slot(splice_addr, s);
                            println!("[DAG] Reclaimed stream slot {}", s);
                        }
                    }
                }
                for s in owned_ios {
                    let key = (SlotKind::Io, s);
                    if let Some(count) = slot_refcounts.get_mut(&key) {
                        *count -= 1;
                        if *count == 0 {
                            reclaimer::free_io_slot(splice_addr, s);
                            println!("[DAG] Reclaimed I/O slot {}", s);
                        }
                    }
                }
                // StreamPipeline: free all intermediate output slots (stages[1..depth-1]).
                // stages[0].arg0 is the pipeline input (freed via node_owned_slots refcount).
                // stages[last].arg1 is the summary output (consumed by downstream nodes).
                if let NodeKind::StreamPipeline(p) = &node.kind {
                    let depth = p.stages.len();
                    for s in p.stages[1..depth.saturating_sub(1)].iter() {
                        if let Some(slot) = s.arg1 {
                            reclaimer::free_stream_slot(splice_addr, slot as usize);
                            println!("[DAG] Reclaimed StreamPipeline internal slot {}", slot);
                        }
                    }
                }

                // Input slots: freed when all direct consumer nodes have run.
                // If an Input node has no consumers at all, free it immediately after it runs.
                if let NodeKind::Input(p) = &node.kind {
                    use common::INPUT_IO_SLOT;
                    if !input_dep_counts.contains_key(node.id.as_str()) {
                        let slot = p.slot.unwrap_or(INPUT_IO_SLOT);
                        reclaimer::free_io_slot(splice_addr, slot as usize);
                        println!("[DAG] Reclaimed Input slot {} (no consumers)", slot);
                    }
                }
                // Decrement the consumer counter for any Input-node dependencies.
                // When the last consumer of an Input node finishes, free the slot.
                let to_free: Vec<(String, u32)> = node.deps.iter()
                    .filter_map(|dep_id| {
                        let entry = input_dep_remaining.get_mut(dep_id.as_str())?;
                        entry.1 -= 1;
                        if entry.1 == 0 { Some((dep_id.clone(), entry.0)) } else { None }
                    })
                    .collect();
                for (dep_id, slot) in to_free {
                    input_dep_remaining.remove(&dep_id);
                    reclaimer::free_io_slot(splice_addr, slot as usize);
                    println!("[DAG] Reclaimed Input slot {} (all consumers done)", slot);
                }
            }

            // After all per-node reclamation in this wave, check whether the
            // free list has grown past the configured threshold and trim it.
            reclaimer::trim_free_list(splice_addr);
        }

        // Drain any orphaned prefetch handles.
        for (id, handle) in prefetch_handles {
            if let Err(e) = handle.join() {
                eprintln!("[DAG] Warning: orphaned prefetch '{}' failed: {}", id, e);
            }
        }

        // Wait for all background persistence writes to complete.
        if let Some(ref mut w) = persist_writer { w.join(); }

        if let Some(ref lg) = logger {
            lg.info("DAG", &format!("run #{} completed", run_count));
        }
        println!("[DAG] All nodes completed (run #{}).", run_count);

        if dag.mode == DagMode::OneShot {
            break;
        }
        // Reset mode: stop if a run limit was specified and we've reached it.
        if dag.runs.map_or(false, |limit| run_count >= limit) {
            println!("[DAG] Reached run limit ({}).", run_count);
            break;
        }
        // Otherwise loop immediately with the same instance and SHM state.
    }

    Ok(())
}
