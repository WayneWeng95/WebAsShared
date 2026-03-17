use serde::Deserialize;

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
    /// Sequential Python pipeline: one persistent `runner.py --loop` process
    /// handles all stages in order, reused for the full node execution.
    /// Equivalent to `StreamPipeline` but for Python workloads — saves the
    /// per-stage Python startup cost compared to individual `PyFunc` nodes.
    PyPipeline(PyPipelineParams),
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

/// One stage in a `PyPipeline`.
#[derive(Debug, Deserialize)]
pub struct PyPipelineStage {
    /// Python workload function name (must exist in `workloads.py`).
    pub func: String,
    /// First argument passed to the function (default 0).
    #[serde(default)]
    pub arg: u32,
    /// Optional second argument.
    #[serde(default)]
    pub arg2: Option<u32>,
}

/// Parameters for the `PyPipeline` node.
///
/// A single persistent `runner.py --loop` process handles every stage call in
/// order.  This avoids paying the Python (or `wasmtime run python.wasm`) startup
/// cost once per stage — instead it is paid just once for the whole pipeline.
///
/// Example (image processing pipeline):
/// ```json
/// { "stages": [
///     { "func": "img_load_ppm"   },
///     { "func": "img_rotate"     },
///     { "func": "img_grayscale"  },
///     { "func": "img_equalize"   },
///     { "func": "img_blur"       },
///     { "func": "img_export_ppm" }
/// ]}
/// ```
#[derive(Debug, Deserialize)]
pub struct PyPipelineParams {
    /// Ordered list of pipeline stages.  Must contain at least one entry.
    pub stages: Vec<PyPipelineStage>,
}
