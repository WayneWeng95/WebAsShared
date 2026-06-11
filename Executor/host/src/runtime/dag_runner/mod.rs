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
mod workers;
mod grouping;
mod pipeline;
mod dispatch;

pub use types::*;

use anyhow::{anyhow, Result};
use serde_json;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use std::fs::OpenOptions;
use wasmtime::*;
use crate::runtime::input_output::slot_loader::{PrefetchHandle, SlotLoader};
use crate::runtime::input_output::logger::HostLogger;
use crate::runtime::mem_operation::reclaimer::{self, SlotKind};
use crate::runtime::worker::{create_wasmtime_engine, setup_vma_environment, WorkerState};
use crate::runtime::input_output::persistence::PersistenceWriter;
use crate::shm::{format_shared_memory, sync_mapping_to_capacity};
use common::WASM_PATH;
use plan::{build_barrier_assignments, build_slot_refcounts, build_waves, is_oneshot_node, node_owned_slots, node_routed_upstream_slots, parse_level, topo_sort, validate_barrier_groups, validate_dag};
use workers::{spawn_python_subprocess, spawn_wasm_subprocess};
use dispatch::execute_node;
use crate::runtime::remote::{execute_remote_recv, execute_remote_send, pre_alloc_staging, STAGE_BYTES_PER_PEER};
use common::{atomic_shm_offset, rdma_scratch_shm_offset, REGISTRY_OFFSET, RegistryEntry};

// ─── Atomic arena helpers (used in wave loop thread spawns) ──────────────────

fn resolve_atomic_index_mod(splice_addr: usize, name: &str) -> Result<usize> {
    use common::Superblock;
    use std::sync::atomic::Ordering;

    let mut name_key = [0u8; 52];
    let src = name.as_bytes();
    name_key[..src.len().min(52)].copy_from_slice(&src[..src.len().min(52)]);

    let sb    = unsafe { &*(splice_addr as *const Superblock) };
    let base  = (splice_addr + REGISTRY_OFFSET as usize) as *const RegistryEntry;
    let count = sb.next_atomic_idx.load(Ordering::Acquire) as usize;
    for i in 0..count {
        let entry = unsafe { &*base.add(i) };
        if entry.name == name_key {
            return Ok(entry.index as usize);
        }
    }
    Err(anyhow!("atomic '{}' not found in registry", name))
}

#[inline]
fn read_shm_atomic(splice_addr: usize, idx: usize) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    let ptr = (splice_addr + atomic_shm_offset(idx) as usize) as *const AtomicU64;
    unsafe { (*ptr).load(Ordering::Acquire) }
}

#[inline]
fn write_shm_atomic(splice_addr: usize, idx: usize, val: u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    let ptr = (splice_addr + atomic_shm_offset(idx) as usize) as *mut AtomicU64;
    unsafe { (*ptr).store(val, Ordering::Release) };
}

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

    // Register the SHM file for host-driven growth (SlotLoader, RDMA receive,
    // and the reclaimer all call shm::try_grow_shm when the bump overflows).
    let splice_addr = store.data().splice_addr;
    crate::shm::register_shm_for_growth(file.try_clone()?, splice_addr);

    let wasm_path = dag.wasm_path.as_deref().unwrap_or(WASM_PATH);
    let module = crate::runtime::worker::load_guest_module(&engine, wasm_path)?;
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

    // Validate and assign intra-wave barrier groups.
    validate_barrier_groups(&dag.nodes, &waves)?;
    let (wave_barriers, barrier_groups) = build_barrier_assignments(&dag.nodes, &waves);
    if !barrier_groups.is_empty() {
        println!("[DAG] Barrier groups:");
        for (name, (bid, count)) in &barrier_groups {
            println!("[DAG]   '{}' → barrier_id={}, party_count={}", name, bid, count);
        }
    }

    // Set up the RDMA full-mesh if any RemoteSend/RemoteRecv nodes are present.
    //
    // Pre-allocate staging pages FIRST (before any DAG nodes run), then
    // register the SHM itself as the RDMA Memory Region via connect_all_on_shm.
    // Both machines pre-allocate the same number of pages from identical
    // initial SHM states (format_shared_memory), so the staging page offsets
    // are byte-for-byte identical on every machine — enabling the receiver to
    // read data that the sender RDMA-WROTEinto the same SHM offset.
    let mesh: Option<std::sync::Arc<connect::MeshNode>> = if let Some(ref rdma) = dag.rdma {
        let splice_addr = store.data().splice_addr;

        // Reserve staging pages only when RDMA data transfer is enabled.
        // Both machines must use the same `transfer` value so the bump
        // allocator advances identically and staging offsets stay in sync.
        if rdma.transfer {
            pre_alloc_staging(splice_addr, rdma.total)?;
        } else {
            println!("[DAG] RDMA transfer disabled — skipping staging pre-alloc");
        }

        // Register the full SHM as the RDMA MR so peers can RDMA-WRITE
        // directly into our staging area, and so RDMA atomics (FAA/CAS)
        // can target arbitrary SHM locations.
        let ip_refs: Vec<&str> = rdma.ips.iter().map(|s| s.as_str()).collect();
        let node = unsafe {
            connect::MeshNode::connect_all_on_shm(
                rdma.node_id,
                rdma.total,
                &ip_refs,
                splice_addr as *mut u8,
                common::INITIAL_SHM_SIZE as usize,
                Some(dag.shm_path.as_str()),
            )
        }.map_err(|e| anyhow!("RDMA mesh setup failed: {}", e))?;

        // If any workload in this DAG is Python, the MR2 memcpy-back path
        // must produce direct-mode PageIds only (Python's shm.py cannot
        // follow paged-mode chains).  Otherwise leave the efficient
        // `reclaimer`-based path with free-list reuse and paged-mode
        // fallback intact.
        let has_python = dag.nodes.iter().any(|n| matches!(n.kind,
            NodeKind::PyFunc(_) | NodeKind::PyPipeline(_)
        ));
        node.set_python_compat(has_python);
        println!(
            "[DAG] RDMA mesh ready (node {} of {}), python_compat={}",
            rdma.node_id, rdma.total, has_python
        );
        Some(std::sync::Arc::new(node))
    } else {
        None
    };

    let has_persistence = dag.nodes.iter().any(|n| matches!(n.kind, NodeKind::Persist(_) | NodeKind::Watch(_)));

    // Build a map from each RemoteRecv node's id to (slot, slot_kind, consumer_count).
    // RemoteRecv-produced slots must be freed after all downstream consumers finish,
    // mirroring the Input slot reclamation pattern.
    //
    // Only IO slots are tracked here: stream slots produced by RemoteRecv are
    // typically consumed by a StreamPipeline that already claims them via
    // node_owned_slots / build_slot_refcounts.  Tracking stream slots here too
    // would cause a double-free when both paths fire.
    let remote_recv_dep_counts: HashMap<String, (usize, RemoteSlotKind, usize)> = {
        let recv_slots: HashMap<&str, (usize, RemoteSlotKind)> = dag.nodes.iter()
            .filter_map(|n| {
                if let NodeKind::RemoteRecv(p) = &n.kind {
                    if p.slot_kind == RemoteSlotKind::Io {
                        return Some((n.id.as_str(), (p.slot, p.slot_kind)));
                    }
                }
                None
            })
            .collect();
        let mut counts: HashMap<String, (usize, RemoteSlotKind, usize)> = HashMap::new();
        for node in &dag.nodes {
            for dep_id in &node.deps {
                if let Some(&(slot, kind)) = recv_slots.get(dep_id.as_str()) {
                    counts.entry(dep_id.clone()).or_insert((slot, kind, 0)).2 += 1;
                }
            }
        }
        counts
    };

    // Set of RemoteRecv node ids that have at least one downstream consumer
    // (any node listing them as a dep), for BOTH Stream and Io slot kinds.
    //
    // The deferred/immediate free logic below only tracks Io recvs in
    // `remote_recv_dep_counts` (stream recvs are freed by whoever consumes
    // them: a StreamPipeline via build_slot_refcounts, or an Aggregate via
    // node_routed_upstream_slots → clear_stream_slot).  But the "no consumers,
    // free immediately" branch must NOT fire for a stream recv that actually
    // has a consumer — otherwise its slot is freed one wave before the consumer
    // (e.g. aggregate_global) reads it, silently dropping the transferred data.
    let remote_recv_has_consumer: HashSet<String> = {
        let recv_ids: HashSet<&str> = dag.nodes.iter()
            .filter_map(|n| matches!(n.kind, NodeKind::RemoteRecv(_)).then_some(n.id.as_str()))
            .collect();
        let mut s = HashSet::new();
        for node in &dag.nodes {
            for dep_id in &node.deps {
                if recv_ids.contains(dep_id.as_str()) {
                    s.insert(dep_id.clone());
                }
            }
        }
        s
    };

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

    // ── Chunked inputs ───────────────────────────────────────────────────────
    // Input nodes with `chunk_bytes` set are loaded one line-aligned chunk per
    // run (instead of whole-file) so inputs larger than the guest SHM window can
    // be processed incrementally.  We drive the run loop until every chunked
    // input reaches EOF; the generic Input dispatch skips these (loaded here).
    struct ChunkedInput { slot: u32, path: String, chunk_bytes: usize, file_len: u64 }
    let chunked_inputs: Vec<ChunkedInput> = dag.nodes.iter().filter_map(|n| {
        if let NodeKind::Input(p) = &n.kind {
            if let Some(cb) = p.chunk_bytes {
                use common::INPUT_IO_SLOT;
                let path = if !p.paths.is_empty() { p.paths[0].clone() } else { p.path.clone() };
                let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                return Some(ChunkedInput { slot: p.slot.unwrap_or(INPUT_IO_SLOT), path, chunk_bytes: cb, file_len });
            }
        }
        None
    }).collect();
    let chunked_mode = !chunked_inputs.is_empty();
    let mut chunk_offsets: Vec<u64> = vec![0; chunked_inputs.len()];
    if chunked_mode {
        let total: u64 = chunked_inputs.iter().map(|c| c.file_len).sum();
        println!("[DAG] chunked input: {} source(s), {:.1} MiB total, ~{} MiB/chunk",
                 chunked_inputs.len(), total as f64 / (1024.0 * 1024.0),
                 chunked_inputs[0].chunk_bytes / (1024 * 1024));
    }

    let mut run_count = 0u32;

    loop {
        run_count += 1;

        // Load the next line-aligned chunk of each chunked input into its slot.
        // `all_chunks_done` becomes true once every input has been fully consumed
        // by (and including) this run, which is then the final run.
        let mut all_chunks_done = chunked_mode;
        if chunked_mode {
            let splice_addr = store.data().splice_addr;
            // A previous run's WASM worker subprocess may have grown the SHM
            // file; re-sync this process's mapping before loading/walking pages.
            sync_mapping_to_capacity(splice_addr)?;
            let loader = SlotLoader::new(splice_addr);
            for (k, c) in chunked_inputs.iter().enumerate() {
                if chunk_offsets[k] < c.file_len {
                    let (_recs, consumed) = loader
                        .load_chunk(std::path::Path::new(&c.path), c.slot, chunk_offsets[k], c.chunk_bytes)?;
                    chunk_offsets[k] += consumed;
                    if consumed == 0 { chunk_offsets[k] = c.file_len; } // guard against stall
                }
                if chunk_offsets[k] < c.file_len { all_chunks_done = false; }
            }
            println!("[DAG] ══ chunk run #{} (offsets {:?} / {:?}) ══",
                     run_count, chunk_offsets,
                     chunked_inputs.iter().map(|c| c.file_len).collect::<Vec<_>>());
        }

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
        // Per-run countdown for Input and RemoteRecv slot reclamation (reset each iteration).
        let mut input_dep_remaining = input_dep_counts.clone();
        let mut remote_recv_dep_remaining = remote_recv_dep_counts.clone();

        // Per-wave timing (compute only — staging/file-replication happens in the
        // node-agent worker before the executor is spawned, so it is already
        // excluded from everything measured here).  See Point (3) in problems.md.
        let compute_start = Instant::now();
        let mut wave_times: Vec<(usize, std::time::Duration, usize)> = Vec::with_capacity(waves.len());

        // Run each wave
        for (wave_idx, wave) in waves.iter().enumerate() {
            let wave_start = Instant::now();
            // 0. Reset barrier counters for groups active in this wave.
            {
                let splice_addr = store.data().splice_addr;
                let sb = unsafe { &*(splice_addr as *const common::Superblock) };
                for &bid in &wave_barriers[wave_idx] {
                    sb.barriers[bid].store(0, std::sync::atomic::Ordering::Release);
                }
            }

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
                .partition(|&&idx| is_oneshot_node(&dag.nodes[idx].kind));

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

            // 3b. Partition host nodes: threaded (RDMA + StreamOutput sink) vs serial.
            //     StreamOutput runs as a thread so its per-round rdma_recv can
            //     progress concurrently with a source StreamPipeline streaming
            //     input out on the main thread (opposite directions of the
            //     streaming lane — conn-3 send / conn-4 recv — so no collision).
            let (rdma_idxs, serial_idxs): (Vec<usize>, Vec<usize>) =
                host_idxs.iter().cloned().partition(|&idx| {
                    matches!(dag.nodes[idx].kind,
                        NodeKind::RemoteSend(_)
                        | NodeKind::RemoteRecv(_)
                        | NodeKind::RemoteAtomicFetchAdd(_)
                        | NodeKind::RemoteAtomicCmpSwap(_)
                        | NodeKind::RemoteAtomicPush(_)
                        | NodeKind::StreamOutput(_))
                });

            // 3c. Spawn RDMA nodes as OS threads so bidirectional transfers
            //     (e.g. shuffle) can progress concurrently without deadlocking.
            let splice_addr = store.data().splice_addr;
            let rdma_threads: Vec<(String, std::thread::JoinHandle<Result<()>>)> =
                rdma_idxs.iter()
                    .map(|&idx| {
                        let node = &dag.nodes[idx];
                        println!("[DAG] ── Node: {} (rdma thread) ──", node.id);
                        let id = node.id.clone();
                        let handle: std::thread::JoinHandle<Result<()>> = match &node.kind {
                            NodeKind::RemoteSend(p) => {
                                let mesh_arc = mesh.as_ref().ok_or_else(|| anyhow!(
                                    "[{}] RemoteSend requires dag.rdma to be configured", id
                                ))?.clone();
                                let ch       = mesh_arc.send_channel(p.peer);
                                let slot     = p.slot;
                                let kind     = p.slot_kind;
                                let protocol = p.protocol;
                                std::thread::spawn(move || {
                                    execute_remote_send(splice_addr, slot, kind, &ch, protocol, &mesh_arc)
                                })
                            }
                            NodeKind::RemoteRecv(p) => {
                                let mesh_arc = mesh.as_ref().ok_or_else(|| anyhow!(
                                    "[{}] RemoteRecv requires dag.rdma to be configured", id
                                ))?.clone();
                                let ch       = mesh_arc.recv_channel(p.peer);
                                let slot     = p.slot;
                                let kind     = p.slot_kind;
                                let protocol = p.protocol;
                                std::thread::spawn(move || {
                                    execute_remote_recv(splice_addr, slot, kind, &ch, protocol, &mesh_arc)
                                })
                            }
                            NodeKind::RemoteAtomicFetchAdd(p) => {
                                let mesh = mesh.as_ref().ok_or_else(|| anyhow!(
                                    "[{}] RemoteAtomicFetchAdd requires dag.rdma to be configured", id
                                ))?;
                                let idx        = resolve_atomic_index_mod(splice_addr, &p.name)?;
                                let ch         = mesh.atomic_channel(p.peer);
                                let remote_off = atomic_shm_offset(idx);
                                let result_off = rdma_scratch_shm_offset(mesh.id, p.peer);
                                let add_val    = p.add;
                                let log_id     = id.clone();
                                std::thread::spawn(move || {
                                    let old = ch.rdma_fetch_add(remote_off, result_off, add_val)?;
                                    write_shm_atomic(splice_addr, idx, old);
                                    println!("[DAG] RemoteAtomicFetchAdd '{}': old={}", log_id, old);
                                    Ok(())
                                })
                            }
                            NodeKind::RemoteAtomicCmpSwap(p) => {
                                let mesh = mesh.as_ref().ok_or_else(|| anyhow!(
                                    "[{}] RemoteAtomicCmpSwap requires dag.rdma to be configured", id
                                ))?;
                                let idx        = resolve_atomic_index_mod(splice_addr, &p.name)?;
                                let ch         = mesh.atomic_channel(p.peer);
                                let remote_off = atomic_shm_offset(idx);
                                let result_off = rdma_scratch_shm_offset(mesh.id, p.peer);
                                let compare    = p.compare;
                                let swap       = p.swap;
                                let log_id     = id.clone();
                                std::thread::spawn(move || {
                                    let old = ch.rdma_compare_swap(remote_off, result_off, compare, swap)?;
                                    write_shm_atomic(splice_addr, idx, old);
                                    println!(
                                        "[DAG] RemoteAtomicCmpSwap '{}': old={}, swapped={}",
                                        log_id, old, old == compare
                                    );
                                    Ok(())
                                })
                            }
                            NodeKind::RemoteAtomicPush(p) => {
                                let mesh = mesh.as_ref().ok_or_else(|| anyhow!(
                                    "[{}] RemoteAtomicPush requires dag.rdma to be configured", id
                                ))?;
                                let idx        = resolve_atomic_index_mod(splice_addr, &p.name)?;
                                let local_val  = read_shm_atomic(splice_addr, idx);
                                let ch         = mesh.atomic_channel(p.peer);
                                let remote_off = atomic_shm_offset(idx);
                                let result_off = rdma_scratch_shm_offset(mesh.id, p.peer);
                                let log_id     = id.clone();
                                std::thread::spawn(move || {
                                    ch.rdma_fetch_add(remote_off, result_off, local_val)?;
                                    println!(
                                        "[DAG] RemoteAtomicPush '{}': pushed {}",
                                        log_id, local_val
                                    );
                                    Ok(())
                                })
                            }
                            NodeKind::StreamOutput(p) => {
                                // Per-round streaming sink.  Owned-clone the params
                                // (paths/rdma_recv) so they outlive the borrow of
                                // `dag.nodes`, and an Option-clone of the mesh Arc
                                // (only needed when `rdma_recv` is set).
                                let params   = p.clone();
                                let mesh_opt = mesh.clone();
                                let node_id  = id.clone();
                                std::thread::spawn(move || {
                                    pipeline::execute_stream_output(
                                        &params, &node_id, splice_addr, mesh_opt.as_ref(),
                                    )
                                })
                            }
                            _ => unreachable!(),
                        };
                        Ok((id, handle))
                    })
                    .collect::<Result<Vec<_>>>()?;

            // 3d. Run serial host nodes on main thread (concurrent with RDMA threads).
            // StreamPipeline / PyPipeline fall here and may use the mesh for per-round
            // rdma_recv / rdma_send — pass mesh.as_ref() so they can access it.
            for &idx in &serial_idxs {
                let node = &dag.nodes[idx];
                println!("[DAG] ── Node: {} ──", node.id);
                execute_node(node, &mut store, &instance, &memory,
                             persist_writer.as_ref().map(|w| w as &PersistenceWriter),
                             logger.as_ref().map(|l| l as &HostLogger),
                             &mut prefetch_handles, (run_count - 1) as usize,
                             &dag.shm_path, py_script, py_wasm, wasm_path,
                             mesh.as_ref())?;
            }

            // 3e. Join RDMA threads before post-wave reclamation.
            for (id, handle) in rdma_threads {
                handle.join()
                    .map_err(|_| anyhow!("[{}] RDMA thread panicked", id))??;
                println!("  [{}] → ok", id);
            }

            // 3g. Wait for all subprocess WASM children.
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
                            // Accumulator slots (chunked-input running totals) survive
                            // across runs — never reclaim them.
                            if dag.persist_slots.contains(&(s as u32)) {
                                println!("[DAG] Persisted stream slot {} (accumulator)", s);
                            } else {
                                reclaimer::free_stream_slot(splice_addr, s);
                                println!("[DAG] Reclaimed stream slot {}", s);
                            }
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
                // Guard: a 1-stage pipeline has no intermediate slots to free.
                if let NodeKind::StreamPipeline(p) = &node.kind {
                    let depth = p.stages.len();
                    for s in p.stages[1..depth.saturating_sub(1).max(1)].iter() {
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

                // Decrement the consumer counter for any RemoteRecv-node dependencies.
                // When the last consumer finishes, free the produced slot (stream or IO).
                let recv_to_free: Vec<(String, usize, RemoteSlotKind)> = node.deps.iter()
                    .filter_map(|dep_id| {
                        let entry = remote_recv_dep_remaining.get_mut(dep_id.as_str())?;
                        entry.2 -= 1;
                        if entry.2 == 0 { Some((dep_id.clone(), entry.0, entry.1)) } else { None }
                    })
                    .collect();
                for (dep_id, slot, kind) in recv_to_free {
                    remote_recv_dep_remaining.remove(&dep_id);
                    match kind {
                        RemoteSlotKind::Stream => {
                            reclaimer::free_stream_slot(splice_addr, slot);
                            println!("[DAG] Reclaimed RemoteRecv stream slot {} (all consumers done)", slot);
                        }
                        RemoteSlotKind::Io => {
                            reclaimer::free_io_slot(splice_addr, slot);
                            println!("[DAG] Reclaimed RemoteRecv I/O slot {} (all consumers done)", slot);
                        }
                    }
                }

                // RemoteRecv with no downstream consumers: free immediately after it runs.
                // A recv WITH consumers is freed by the consumer instead (Io: the deferred
                // remote_recv_dep_remaining countdown above; Stream: the consuming
                // StreamPipeline's refcount or the Aggregate's clear_stream_slot), so the
                // slot stays alive until that consumer has read it.
                if let NodeKind::RemoteRecv(p) = &node.kind {
                    if !remote_recv_has_consumer.contains(node.id.as_str()) {
                        match p.slot_kind {
                            RemoteSlotKind::Stream => {
                                reclaimer::free_stream_slot(splice_addr, p.slot);
                                println!("[DAG] Reclaimed RemoteRecv stream slot {} (no consumers)", p.slot);
                            }
                            RemoteSlotKind::Io => {
                                reclaimer::free_io_slot(splice_addr, p.slot);
                                println!("[DAG] Reclaimed RemoteRecv I/O slot {} (no consumers)", p.slot);
                            }
                        }
                    }
                }
            }

            // After all per-node reclamation in this wave, check whether the
            // free list has grown past the configured threshold and trim it.
            reclaimer::trim_free_list(splice_addr);

            wave_times.push((wave_idx, wave_start.elapsed(), wave.len()));
        }

        // ── Per-wave timing summary (compute only, staging excluded) ───────────
        let compute_ms = compute_start.elapsed().as_secs_f64() * 1e3;
        println!("[DAG][timing] ── run #{} compute breakdown (staging excluded) ──", run_count);
        for (wi, dur, n) in &wave_times {
            let ms = dur.as_secs_f64() * 1e3;
            let pct = if compute_ms > 0.0 { ms / compute_ms * 100.0 } else { 0.0 };
            let slowest = waves[*wi].iter()
                .map(|&i| dag.nodes[i].id.as_str())
                .collect::<Vec<_>>().join(",");
            let label = if slowest.len() > 60 { format!("{}…", &slowest[..60]) } else { slowest };
            println!("[DAG][timing]   wave {:>2}: {:>8.2} ms ({:>4.1}%)  {} node(s)  [{}]",
                     wi, ms, pct, n, label);
        }
        println!("[DAG][timing]   TOTAL compute: {:.2} ms across {} wave(s)", compute_ms, wave_times.len());
        if let Some(ref lg) = logger {
            lg.info("DAG", &format!("run #{} compute {:.2} ms across {} waves", run_count, compute_ms, wave_times.len()));
            for (wi, dur, n) in &wave_times {
                lg.info("DAG", &format!("  wave {} {:.2} ms {} nodes", wi, dur.as_secs_f64() * 1e3, n));
            }
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

        // Chunked input drives the loop: keep running until every input hits EOF,
        // regardless of mode.
        if chunked_mode {
            if all_chunks_done {
                println!("[DAG] All input chunks processed ({} runs).", run_count);
                break;
            }
            // Reclaim ALL transient slots before the next chunk so per-chunk data
            // (distribute partitions, map inputs, the input chunk) does NOT carry
            // over and get re-counted — only `persist_slots` accumulators survive.
            let splice_addr = store.data().splice_addr;
            // Re-sync the mapping first: a worker subprocess may have grown the
            // SHM, so chains can extend past this process's stale VMA.
            sync_mapping_to_capacity(splice_addr)?;
            reclaimer::free_all_transient(splice_addr, &dag.persist_slots);
            continue;
        }

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
