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

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use wasmtime::*;

use std::path::{Path, PathBuf};

use crate::policy::{EqualSlice, FixedMapPartition, FixedSizeSlice, LineBoundarySlice, ModuloPartition, RoundRobinPartition};
use crate::routing::aggregate::AggregateConnection;
use crate::routing::broadcast::BroadcastConnection;
use crate::routing::dispatch::{FileDispatcher, OwnedSlice};
use crate::routing::shuffle::ShuffleConnection;
use crate::routing::stream::HostStream;
use crate::runtime::inputer::{load_file, Inputer, PrefetchHandle};
use crate::runtime::logger::{HostLogger, Level};
use crate::runtime::outputer::Outputer;
use crate::runtime::reclaimer::{self, SlotKind};
use crate::runtime::slicer::Slicer;
use crate::runtime::worker::{create_wasmtime_engine, setup_vma_environment, WorkerState};
use crate::runtime::writer::{PersistenceOptions, PersistenceWriter};
use crate::shm::format_shared_memory;
use common::WASM_PATH;

// ─── JSON schema ─────────────────────────────────────────────────────────────

/// Execution mode for a DAG.
///
/// - `"one_shot"` (default) — execute once and exit.
/// - `"reset"` — after all nodes complete, re-execute from the beginning
///   using the same WASM instance and SHM connection.  Send SIGINT (Ctrl-C)
///   to stop.
#[derive(Debug, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DagMode {
    #[default]
    OneShot,
    Reset,
}

#[derive(Debug, Deserialize)]
pub struct Dag {
    /// Path to the SHM file; created and formatted automatically.
    pub shm_path: String,
    /// Optional path to the WASM module.  Defaults to `WASM_PATH` (guest.wasm).
    #[serde(default)]
    pub wasm_path: Option<String>,
    /// Path to the Python runner script (runner.py).  Required when any node
    /// uses `PyFunc`.  Relative paths are resolved from the process working dir.
    #[serde(default)]
    pub python_script: Option<String>,
    /// Optional path to a `python.wasm` binary.  When set, `PyFunc` nodes are
    /// executed via `wasmtime run <python_wasm> -- <python_script>` instead of
    /// the host's native `python3`.  Requires `wasmtime` to be on PATH.
    /// The SHM parent directory and the script directory are automatically
    /// mounted as WASI preopens so the guest can access both.
    #[serde(default)]
    pub python_wasm: Option<String>,
    /// Optional log level for host-side SHM logging.
    /// Accepted: `"debug"`, `"info"`, `"warn"`, `"error"`, `"off"` (default).
    #[serde(default)]
    pub log_level: Option<String>,
    /// Execution mode: `"one_shot"` (default) or `"reset"`.
    #[serde(default)]
    pub mode: DagMode,
    /// Maximum number of runs in `reset` mode.  Omit (or set to `null`) for
    /// an infinite loop.  Ignored in `one_shot` mode.
    #[serde(default)]
    pub runs: Option<u32>,
    pub nodes: Vec<DagNode>,
}

#[derive(Debug, Deserialize)]
pub struct DagNode {
    /// Unique node identifier used in `deps` references.
    pub id: String,
    /// IDs of nodes that must complete before this one runs.
    #[serde(default)]
    pub deps: Vec<String>,
    pub kind: NodeKind,
}

#[derive(Debug, Deserialize)]
pub enum NodeKind {
    /// Call `func(arg) -> ()` — fire-and-forget WASM invocation.
    WasmVoid(WasmCall),
    /// Call `func(arg) -> u32` — result is logged.
    WasmU32(WasmCall),
    /// Call `func(arg) -> u64` — fat-pointer result decoded and printed.
    WasmFatPtr(WasmCall),
    /// `HostStream::bridge(from, to)` — zero-copy 1→1 chain redirect.
    Bridge(BridgeParams),
    /// `AggregateConnection::new(upstream, downstream).bridge()` — N→1 merge.
    Aggregate(AggregateParams),
    /// `ShuffleConnection::new(upstream, downstream, policy).bridge()` — N→M routing.
    Shuffle(ShuffleParams),
    /// Snapshot SHM data and flush to storage in a background thread.
    Persist(PersistParams),
    /// Lightweight: persist a single stream slot or a single shared-state entry.
    Watch(WatchParams),
    /// Multi-round streaming pipeline: source → filter → transform → sink,
    /// each stage advancing its own SHM-atomic cursor each round.
    StreamPipeline(StreamPipelineParams),
    /// Load a file, slice it with a policy, and dispatch slices to N workers.
    FileDispatch(FileDispatchParams),
    /// Dispatch a list of inline owned byte payloads to N workers.
    OwnedDispatch(OwnedDispatchParams),
    /// Read the reserved output slot and save all records to a file.
    Output(OutputParams),
    /// Free specified stream and/or I/O slot page chains back to the SHM pool.
    /// Use between sequential pipeline runs that reuse the same fixed slots.
    FreeSlots(FreeSlotsParams),
    /// Load a file from `path` and write its content into the reserved input
    /// slot (INPUT_SLOT_ID), one record per non-empty line.  The guest reads
    /// the records via `ShmApi::read_input` / `ShmApi::read_all_inputs`.
    Input(InputParams),
    /// Spawn the Python runner script with `WORKLOAD_FUNC` / `WORKLOAD_ARG`
    /// env vars.  Uses the host's native `python3` (or a pre-built
    /// `python.wasm` via `wasmtime run`) specified in the DAG `python_script`.
    PyFunc(PyFuncCall),
}

#[derive(Debug, Deserialize)]
pub struct WasmCall {
    pub func: String,
    pub arg: u32,
}

#[derive(Debug, Deserialize)]
pub struct PyFuncCall {
    pub func: String,
    #[serde(default)]
    pub arg: u32,
    /// Optional second argument for two-parameter workload functions.
    #[serde(default)]
    pub arg2: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct BridgeParams {
    pub from: usize,
    pub to: usize,
}

#[derive(Debug, Deserialize)]
pub struct AggregateParams {
    pub upstream: Vec<usize>,
    pub downstream: usize,
}

#[derive(Debug, Deserialize)]
pub struct ShuffleParams {
    pub upstream: Vec<usize>,
    pub downstream: Vec<usize>,
    pub policy: PolicySpec,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum PolicySpec {
    Modulo,
    RoundRobin,
    FixedMap {
        /// Each inner array is `[upstream_id, slot_index]`.
        map: Vec<[usize; 2]>,
        #[serde(default)]
        default_slot: usize,
    },
    /// Fan-out: every upstream is merged into every downstream slot.
    Broadcast,
}

/// Lightweight single-item watch: persists exactly one stream slot or one
/// named shared-state entry to the given output file path.
/// Set exactly one of `stream` or `shared`; the other must be absent.
#[derive(Debug, Deserialize)]
pub struct WatchParams {
    /// Exact output file path (parent directory is created if absent).
    pub output: String,
    /// Stream slot ID to persist (mutually exclusive with `shared`).
    pub stream: Option<usize>,
    /// Named shared-state entry to persist (mutually exclusive with `stream`).
    pub shared: Option<String>,
}

/// One stage in a `StreamPipeline`.
///
/// The host calls `func(arg0, arg1_resolved)` as an isolated subprocess each
/// tick the stage is active.  When `arg1` is `None` the host injects the
/// current round number in its place — this is the convention for source
/// stages that need to know which batch to produce.
#[derive(Debug, Deserialize)]
pub struct PipelineStage {
    /// Exported WASM function name.
    pub func: String,
    /// First argument (typically the input slot).
    pub arg0: u32,
    /// Second argument.  `null` / `None` → inject the current round number.
    pub arg1: Option<u32>,
}

/// Parameters for the `StreamPipeline` node.
///
/// Stages are executed in a pipelined wave schedule so adjacent rounds
/// overlap.  For N rounds and D stages the total ticks = N + D − 1 instead
/// of N × D for a naïve serial approach.
///
/// Per-stage cursors live in SHM atomics so they survive the process
/// boundary between ticks.
///
/// Example (4-stage word-count pipeline):
/// ```json
/// { "rounds": 8, "stages": [
///     { "func": "pipeline_source",    "arg0": 10, "arg1": null },
///     { "func": "pipeline_filter",    "arg0": 10, "arg1": 20 },
///     { "func": "pipeline_transform", "arg0": 20, "arg1": 30 },
///     { "func": "pipeline_sink",      "arg0": 30, "arg1": 40 }
/// ]}
/// ```
#[derive(Debug, Deserialize)]
pub struct StreamPipelineParams {
    /// Number of rounds to execute.
    pub rounds: u32,
    /// Ordered list of pipeline stages.  Must contain at least one entry.
    pub stages: Vec<PipelineStage>,
}

#[derive(Debug, Deserialize)]
pub struct PersistParams {
    /// Directory where output files are written (created if absent).
    pub output_dir: String,
    /// Save all named atomic variables from the registry.
    #[serde(default)]
    pub atomics: bool,
    /// Stream slot IDs whose records should be persisted.
    #[serde(default)]
    pub stream_slots: Vec<usize>,
    /// Save all Manager-committed shared-state entries from the registry.
    #[serde(default)]
    pub shared_state: bool,
}

/// Slicing policy selector for `FileDispatch` nodes.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum SlicePolicySpec {
    /// Split into at most `workers` equal byte ranges.
    Equal,
    /// Split on newlines; never cuts a line.
    LineBoundary,
    /// Split into chunks of at most `max_bytes` bytes.
    FixedSize { max_bytes: usize },
}

/// Load a file from `path`, slice it according to `policy`, and dispatch the
/// resulting `FileSlice`s to `workers` parallel workers.  Each worker logs
/// its assignment summary.
#[derive(Debug, Deserialize)]
pub struct FileDispatchParams {
    /// Path to the file to load and slice.
    pub path: String,
    /// Number of parallel workers.
    pub workers: usize,
    /// How to divide the file into slices.
    pub policy: SlicePolicySpec,
}

/// Dispatch a list of inline string payloads as `OwnedSlice`s to `workers`
/// parallel workers.  Exercises the generic `DispatchSlice` path without
/// requiring an on-disk file.
#[derive(Debug, Deserialize)]
pub struct OwnedDispatchParams {
    /// Number of parallel workers.
    pub workers: usize,
    /// Payload strings; each becomes one `OwnedSlice` in index order.
    pub items: Vec<String>,
}

/// Read all records from a stream slot (written by the guest via
/// `ShmApi::write_output` / `write_output_to`) and save them to `path`,
/// one record per line.
#[derive(Debug, Deserialize)]
pub struct OutputParams {
    /// Destination file path (parent directories are created if absent).
    /// Ignored when `paths` is non-empty.
    #[serde(default)]
    pub path: String,
    /// Per-iteration paths for `reset` mode.  On run N the path used is
    /// `paths[N % paths.len()]`.  Takes priority over `path` when non-empty.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Source I/O slot.  Defaults to `OUTPUT_IO_SLOT` when omitted.
    #[serde(default)]
    pub slot: Option<u32>,
}

/// Explicitly free a set of stream and/or I/O slots, returning their page
/// chains to the SHM pool.  Use this between sequential pipeline runs that
/// reuse the same fixed slot numbers, so the second run starts with empty slots.
///
/// ```json
/// { "kind": { "FreeSlots": { "stream": [20, 30, 40], "io": [10] } } }
/// ```
#[derive(Debug, Deserialize)]
pub struct FreeSlotsParams {
    /// Stream slot IDs whose page chains should be freed.
    #[serde(default)]
    pub stream: Vec<usize>,
    /// I/O slot IDs whose page chains should be freed.
    #[serde(default)]
    pub io: Vec<usize>,
}

/// Load a file into a stream slot so the guest can consume it via
/// `ShmApi::read_all_inputs_from(slot)` (or the default `read_all_inputs()`
/// when the slot is `INPUT_SLOT_ID`).
#[derive(Debug, Deserialize)]
pub struct InputParams {
    /// Path to the file to load (must exist and be readable).
    /// Ignored when `paths` is non-empty.
    #[serde(default)]
    pub path: String,
    /// When `binary` is false (default): on run N the path used is
    /// `paths[N % paths.len()]` — one file per run.
    /// When `binary` is true: ALL paths are loaded on every run, each as one
    /// record, enabling cursor-based per-record consumption by the guest.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Target I/O slot.  Defaults to `INPUT_IO_SLOT` when omitted.
    #[serde(default)]
    pub slot: Option<u32>,
    /// If `true`, load the file in a background thread so subsequent nodes can
    /// run while I/O is in progress.  Use `PrefetchHandle::join` (done
    /// automatically by the DAG executor when the first dependent node is
    /// about to execute) to ensure the data is ready before it is consumed.
    #[serde(default)]
    pub prefetch: bool,
    /// If `true`, load each file as a single binary record instead of
    /// line-by-line.  All paths in `paths` are loaded on every run, producing
    /// one SHM record per file.  Guests consume them with `read_next_io_record`.
    #[serde(default)]
    pub binary: bool,
}

// ─── Logger helpers ───────────────────────────────────────────────────────────

/// Parse a user-supplied level string into a `Level`.
/// Returns `None` for `"off"` or an unrecognised value (logging disabled).
fn parse_level(s: &str) -> Option<Level> {
    match s.to_ascii_lowercase().as_str() {
        "debug" => Some(Level::Debug),
        "info"  => Some(Level::Info),
        "warn"  => Some(Level::Warn),
        "error" => Some(Level::Error),
        _       => None,
    }
}

// ─── Slot bounds validation ───────────────────────────────────────────────────

/// Verify that all explicitly declared stream slot IDs are within
/// `[0, STREAM_SLOT_COUNT)` and all I/O slot IDs are within `[0, IO_SLOT_COUNT)`.
///
/// Stream slots and I/O slots are now completely separate — no slot in either
/// range is reserved for framework use, so the only constraint is that IDs
/// stay in bounds.
fn validate_dag(dag: &Dag) -> Result<()> {
    use common::{IO_SLOT_COUNT, STREAM_SLOT_COUNT};
    let mut errors: Vec<String> = Vec::new();

    for node in &dag.nodes {
        // Collect stream slot IDs declared by routing/pipeline/watch/persist nodes.
        let mut stream_slots: Vec<usize> = Vec::new();
        // Collect I/O slot IDs declared by Input/Output nodes.
        let mut io_slots: Vec<(usize, &str)> = Vec::new(); // (slot, kind_label)

        match &node.kind {
            NodeKind::Bridge(p) => {
                stream_slots.push(p.from);
                stream_slots.push(p.to);
            }
            NodeKind::Aggregate(p) => {
                stream_slots.extend_from_slice(&p.upstream);
                stream_slots.push(p.downstream);
            }
            NodeKind::Shuffle(p) => {
                stream_slots.extend_from_slice(&p.upstream);
                stream_slots.extend_from_slice(&p.downstream);
            }
            NodeKind::StreamPipeline(p) => {
                for s in &p.stages {
                    stream_slots.push(s.arg0 as usize);
                    if let Some(a1) = s.arg1 { stream_slots.push(a1 as usize); }
                }
            }
            NodeKind::Watch(p) => {
                if let Some(s) = p.stream { stream_slots.push(s); }
            }
            NodeKind::Persist(p) => {
                stream_slots.extend_from_slice(&p.stream_slots);
            }
            NodeKind::Input(p) => {
                if let Some(s) = p.slot { io_slots.push((s as usize, "Input")); }
            }
            NodeKind::Output(p) => {
                if let Some(s) = p.slot { io_slots.push((s as usize, "Output")); }
            }
            _ => {}
        }

        for s in stream_slots {
            if s >= STREAM_SLOT_COUNT {
                errors.push(format!(
                    "node '{}': stream slot {} ≥ STREAM_SLOT_COUNT ({})",
                    node.id, s, STREAM_SLOT_COUNT
                ));
            }
        }
        for (s, label) in io_slots {
            if s >= IO_SLOT_COUNT {
                errors.push(format!(
                    "node '{}': {} I/O slot {} ≥ IO_SLOT_COUNT ({})",
                    node.id, label, s, IO_SLOT_COUNT
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("DAG validation failed:\n  {}", errors.join("\n  ")))
    }
}

// ─── Slot lifetime tracking ───────────────────────────────────────────────────

/// Slots whose **metadata only** should be zeroed after `node` finishes.
///
/// Routing operations (Bridge, Aggregate, Shuffle, Broadcast) splice the
/// upstream page chains into downstream chains via `next_offset` links.
/// After routing the upstream slot's `writer_heads`/`writer_tails` still
/// point into pages that are now owned by the downstream chain.  We must
/// zero only the metadata — `clear_stream_slot` — not free the pages;
/// freeing would corrupt the downstream chain and cause the walker in
/// `free_page_chain` to chase into the free-list or into reallocated pages.
fn node_routed_upstream_slots(kind: &NodeKind) -> Vec<usize> {
    match kind {
        NodeKind::Bridge(p)    => vec![p.from],
        NodeKind::Aggregate(p) => p.upstream.clone(),
        NodeKind::Shuffle(p)   => p.upstream.clone(),
        _ => vec![],
    }
}

/// Slots whose **pages should be freed** after `node` finishes.
///
/// Only slots with *exclusive* page ownership are listed here — i.e. no
/// routing operation has spliced those pages into another slot's chain.
///
/// - I/O Output slots: written by the Inputer, read by the guest, drained
///   by the Outputer.  No routing ever touches the I/O area.
/// - StreamPipeline `source_slot`: written by an upstream node and read
///   only by this pipeline; ownership is unambiguous.
///
/// Stream slots involved in routing use `node_routed_upstream_slots` instead.
/// Watch/Persist read stream slots but do not own them, so they are skipped
/// (they are freed by whichever node actually consumes the data).
fn node_owned_slots(kind: &NodeKind) -> (Vec<usize>, Vec<usize>) {
    use common::OUTPUT_IO_SLOT;
    // (stream_slots_to_free, io_slots_to_free)
    match kind {
        NodeKind::Output(p) =>
            (vec![], vec![p.slot.unwrap_or(OUTPUT_IO_SLOT) as usize]),
        NodeKind::StreamPipeline(p) =>
            // stages[0].arg0 is the pipeline's source slot, owned by the upstream node.
            (p.stages.first().map(|s| vec![s.arg0 as usize]).unwrap_or_default(), vec![]),
        _ => (vec![], vec![]),
    }
}

/// Scan the full DAG and build reader-count maps for slots that will be freed.
///
/// Only counts slots tracked by `node_owned_slots` — slots with exclusive
/// page ownership.  Routing upstreams are counted separately via
/// `node_routed_upstream_slots` (they only need a metadata clear, not a free).
fn build_slot_refcounts(dag: &Dag) -> HashMap<(SlotKind, usize), usize> {
    let mut counts: HashMap<(SlotKind, usize), usize> = HashMap::new();
    for node in &dag.nodes {
        let (streams, ios) = node_owned_slots(&node.kind);
        for s in streams {
            *counts.entry((SlotKind::Stream, s)).or_insert(0) += 1;
        }
        for s in ios {
            *counts.entry((SlotKind::Io, s)).or_insert(0) += 1;
        }
    }
    counts
}

// ─── Topological sort (Kahn's algorithm) ─────────────────────────────────────

fn topo_sort(nodes: &[DagNode]) -> Result<Vec<usize>> {
    let id_to_idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();

    let mut in_degree = vec![0usize; nodes.len()];
    // adj[i] = list of nodes that depend on node i (i must finish before them)
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];

    for (i, node) in nodes.iter().enumerate() {
        for dep_id in &node.deps {
            let dep_idx = *id_to_idx
                .get(dep_id.as_str())
                .ok_or_else(|| anyhow!("Unknown dep '{}' in node '{}'", dep_id, node.id))?;
            adj[dep_idx].push(i);
            in_degree[i] += 1;
        }
    }

    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, &d)| d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut order = Vec::with_capacity(nodes.len());
    while let Some(n) = queue.pop_front() {
        order.push(n);
        for &m in &adj[n] {
            in_degree[m] -= 1;
            if in_degree[m] == 0 {
                queue.push_back(m);
            }
        }
    }

    if order.len() != nodes.len() {
        return Err(anyhow!("DAG contains a cycle — cannot execute"));
    }
    Ok(order)
}

// ─── Wave builder and WASM node classifier ───────────────────────────────────

/// Compute execution waves: groups of nodes that can run concurrently.
/// All nodes in a wave have all their dependencies in earlier waves.
fn build_waves(nodes: &[DagNode], order: &[usize]) -> Vec<Vec<usize>> {
    let id_to_idx: HashMap<&str, usize> = nodes.iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let mut level = vec![0usize; nodes.len()];
    for &idx in order {
        for dep_id in &nodes[idx].deps {
            if let Some(&dep_idx) = id_to_idx.get(dep_id.as_str()) {
                level[idx] = level[idx].max(level[dep_idx] + 1);
            }
        }
    }
    let max_level = level.iter().copied().max().unwrap_or(0);
    let mut waves: Vec<Vec<usize>> = vec![Vec::new(); max_level + 1];
    for &idx in order {
        waves[level[idx]].push(idx);
    }
    waves
}

/// Returns true for node kinds executed as isolated subprocesses (WasmVoid/WasmU32/WasmFatPtr).
fn is_subprocess_wasm_node(kind: &NodeKind) -> bool {
    matches!(kind,
        NodeKind::WasmVoid(_)
        | NodeKind::WasmU32(_)
        | NodeKind::WasmFatPtr(_)
    )
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

            // 2. Partition wave: subprocess WASM vs host (routing + StreamPipeline).
            let (sub_idxs, host_idxs): (Vec<usize>, Vec<usize>) = wave.iter()
                .partition(|&&idx| is_subprocess_wasm_node(&dag.nodes[idx].kind));

            if wave.len() > 1 {
                println!("[DAG] Wave: {} nodes in parallel ({} subprocess + {} host)",
                    wave.len(), sub_idxs.len(), host_idxs.len());
            }

            // 3a. Spawn all subprocess WASM nodes in parallel.
            let mut children: Vec<(String, std::process::Child)> = sub_idxs
                .iter()
                .map(|&idx| {
                    let node = &dag.nodes[idx];
                    println!("[DAG] ── Node: {} ──", node.id);
                    let child = spawn_wasm_subprocess(node, dag.shm_path.as_str(), wasm_path)?;
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

// ─── Subprocess WASM helpers ──────────────────────────────────────────────────

/// Spawns `./host wasm-call <shm_path> <wasm_path> <func> <ret_type> <arg>` as a
/// child process and returns the handle.  The caller must `.wait()` on it.
fn spawn_wasm_subprocess(
    node: &DagNode,
    shm_path: &str,
    wasm_path: &str,
) -> Result<std::process::Child> {
    let (func, ret_type, arg) = match &node.kind {
        NodeKind::WasmVoid(c)   => (c.func.as_str(), "void",   c.arg),
        NodeKind::WasmU32(c)    => (c.func.as_str(), "u32",    c.arg),
        NodeKind::WasmFatPtr(c) => (c.func.as_str(), "fatptr", c.arg),
        _ => return Err(anyhow!("[{}] not a subprocess WASM node", node.id)),
    };
    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("cannot find current exe: {}", e))?;
    std::process::Command::new(exe)
        .arg("wasm-call")
        .arg(shm_path)
        .arg(wasm_path)
        .arg(func)
        .arg(ret_type)
        .arg(arg.to_string())
        .spawn()
        .map_err(|e| anyhow!("[{}] failed to spawn WASM worker: {}", node.id, e))
}

/// Spawns `./host wasm-call <shm_path> <wasm_path> <func> void2 <arg0> <arg1>` as a child
/// process for a single StreamPipeline stage and returns the handle.
fn spawn_pipeline_stage(
    func: &str,
    arg0: u32,
    arg1: u32,
    shm_path: &str,
    wasm_path: &str,
    node_id: &str,
) -> Result<std::process::Child> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("cannot find current exe: {}", e))?;
    std::process::Command::new(exe)
        .args(["wasm-call", shm_path, wasm_path, func, "void2",
               &arg0.to_string(), &arg1.to_string()])
        .spawn()
        .map_err(|e| anyhow!("[{}] failed to spawn {}: {}", node_id, func, e))
}

// ─── Node executor ────────────────────────────────────────────────────────────

fn execute_node(
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
        // Active stages within a tick are independent and run as parallel
        // subprocesses; the tick barrier ensures each stage only sees data
        // committed by the previous stage in the prior tick.
        //
        //   tick 0:  [stage·0·r0]
        //   tick 1:  [stage·0·r1]  [stage·1·r0]
        //   tick 2:  [stage·0·r2]  [stage·1·r1]  [stage·2·r0]
        //   ...
        //
        // Total ticks = rounds + depth − 1.
        NodeKind::StreamPipeline(p) => {
            let rounds = p.rounds as usize;
            let depth  = p.stages.len();
            if depth == 0 {
                return Err(anyhow!("[{}] StreamPipeline has no stages", node.id));
            }
            let slot_chain: Vec<String> = p.stages.iter()
                .map(|s| s.arg1.map_or_else(|| format!("{}(r)", s.arg0), |a| format!("{}→{}", s.arg0, a)))
                .collect();
            println!("  StreamPipeline: {} rounds, {} stages | {}",
                rounds, depth, slot_chain.join(" › "));
            log(&format!("pipeline {} rounds {} stages", rounds, depth));

            let total_ticks = rounds + depth - 1;
            for tick in 0..total_ticks {
                let mut children: Vec<(&str, std::process::Child)> = Vec::new();

                for (s_idx, stage) in p.stages.iter().enumerate() {
                    let tick_round = tick as isize - s_idx as isize;
                    if tick_round >= 0 && (tick_round as usize) < rounds {
                        // arg1 = None → inject round number; Some(v) → fixed slot arg
                        let a1 = stage.arg1.unwrap_or(tick_round as u32);
                        children.push((stage.func.as_str(),
                            spawn_pipeline_stage(&stage.func, stage.arg0, a1, shm_path, wasm_path, &node.id)?));
                    }
                }

                for (stage_func, mut child) in children {
                    let status = child.wait()
                        .map_err(|e| anyhow!("[{}] {} wait: {}", node.id, stage_func, e))?;
                    if !status.success() {
                        return Err(anyhow!("[{}] {} failed", node.id, stage_func));
                    }
                }
                println!("    tick {} complete", tick);
                log_debug(&format!("tick {} complete", tick));
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

        // ── PyFunc: spawn Python runner with env vars ─────────────────────────
        NodeKind::PyFunc(call) => {
            if python_script.is_empty() {
                return Err(anyhow!(
                    "[{}] PyFunc requires 'python_script' to be set in the DAG JSON",
                    node.id
                ));
            }
            log_debug(&format!("PyFunc {}({})", call.func, call.arg));

            let status = if let Some(wasm_path) = python_wasm {
                // ── WASM execution via `wasmtime run` ──────────────────────
                // Mount the SHM parent dir and the script dir as WASI preopens
                // so the guest can open both the SHM file and the Python modules.
                let script_dir = Path::new(python_script)
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_string_lossy();
                let shm_dir = Path::new(shm_path)
                    .parent()
                    .unwrap_or(Path::new("/dev/shm"))
                    .to_string_lossy();

                // Resolve the wasmtime binary: prefer the WASMTIME env var,
                // then $HOME/.wasmtime/bin/wasmtime, then fall back to PATH.
                let wasmtime_bin = std::env::var("WASMTIME").unwrap_or_else(|_| {
                    let candidate = std::env::var("HOME")
                        .map(|h| format!("{}/.wasmtime/bin/wasmtime", h))
                        .unwrap_or_default();
                    if !candidate.is_empty() && std::path::Path::new(&candidate).exists() {
                        candidate
                    } else {
                        "wasmtime".to_owned()
                    }
                });
                let mut cmd = std::process::Command::new(&wasmtime_bin);
                cmd.arg("run")
                    .arg("--env").arg(format!("SHM_PATH={}", shm_path))
                    .arg("--env").arg(format!("WORKLOAD_FUNC={}", call.func))
                    .arg("--env").arg(format!("WORKLOAD_ARG={}", call.arg));
                if let Some(a2) = call.arg2 {
                    cmd.arg("--env").arg(format!("WORKLOAD_ARG2={}", a2));
                }
                // Mount SHM directory (may equal script_dir for some setups).
                cmd.arg("--dir").arg(shm_dir.as_ref());
                // Mount script directory (skipped if same as shm_dir to avoid
                // duplicate preopen warnings from some wasmtime versions).
                if script_dir != shm_dir {
                    cmd.arg("--dir").arg(script_dir.as_ref());
                }
                cmd.arg(wasm_path)
                    .arg("--")
                    .arg(python_script);

                println!("  PyFunc {}({}) via python.wasm", call.func, call.arg);
                cmd.status()
                    .map_err(|e| anyhow!("[{}] failed to spawn wasmtime: {}", node.id, e))?
            } else {
                // ── Native execution via `python3` ─────────────────────────
                let mut cmd = std::process::Command::new("python3");
                cmd.arg(python_script)
                    .env("SHM_PATH",      shm_path)
                    .env("WORKLOAD_FUNC", &call.func)
                    .env("WORKLOAD_ARG",  call.arg.to_string());
                if let Some(a2) = call.arg2 {
                    cmd.env("WORKLOAD_ARG2", a2.to_string());
                }
                cmd.status()
                    .map_err(|e| anyhow!("[{}] failed to spawn python3: {}", node.id, e))?
            };

            if !status.success() {
                return Err(anyhow!(
                    "[{}] Python exited with status {}", node.id, status
                ));
            }
            println!("  PyFunc {}({}) → ok", call.func, call.arg);
            log(&format!("PyFunc {}({}) done", call.func, call.arg));
        }
    }

    Ok(())
}
