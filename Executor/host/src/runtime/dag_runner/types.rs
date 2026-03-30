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

/// RDMA full-mesh configuration — required when any node uses `RemoteSend`
/// or `RemoteRecv`.  All machines must include an identical `ips` list and
/// set `node_id` to their own index.
#[derive(Debug, Deserialize)]
pub struct RdmaConfig {
    /// Index of this node in the mesh (0-based).
    pub node_id: usize,
    /// Total number of nodes in the mesh.
    pub total: usize,
    /// IP address or hostname of each node, in index order.
    pub ips: Vec<String>,
    /// Enable RDMA data transfer for `RemoteSend` / `RemoteRecv` nodes.
    ///
    /// When `true` (default) the SHM is registered as the RDMA Memory Region
    /// and a staging area is pre-allocated so slot data can be transferred via
    /// one-sided RDMA WRITE with no TCP memcopy.
    ///
    /// Set to `false` to use the mesh only for RDMA atomic operations
    /// (`fetch_and_add`, `compare_and_swap`) without the staging overhead.
    /// `RemoteSend` / `RemoteRecv` nodes will fail at runtime when `false`.
    #[serde(default = "default_transfer")]
    pub transfer: bool,
}

fn default_transfer() -> bool { true }

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
    /// Optional RDMA full-mesh configuration.  Required when any node uses
    /// `RemoteSend` or `RemoteRecv`.  All nodes in the mesh must specify
    /// matching `total` / `ips` lists and distinct `node_id` values.
    #[serde(default)]
    pub rdma: Option<RdmaConfig>,
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
    WasmVoid(WasmCallParams),
    /// Call `func(arg) -> u32` — result is logged.
    WasmU32(WasmCallParams),
    /// Call `func(arg) -> u64` — fat-pointer result decoded and printed.
    WasmFatPtr(WasmCallParams),
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
    PyFunc(PyFuncParams),
        /// Sequential WASM grouping: one persistent `wasm-loop` subprocess per stage,
    /// all stages called in order with no pipelining across rounds.
    /// Saves the per-call wasmtime JIT cost compared to individual `WasmVoid` nodes
    /// while keeping execution strictly sequential within a single DAG node.
    WasmGrouping(WasmGroupingParams),
    /// Sequential Python grouping: one persistent `runner.py --loop` process
    /// handles all stages in order, reused for the full node execution.
    /// Saves the per-stage Python startup cost compared to individual `PyFunc` nodes.
    /// Renamed from `PyPipeline` — use `PyPipeline` for true pipelined execution
    /// across multiple rounds.
    PyGrouping(PyGroupingParams),
    /// True pipelined Python execution: one persistent `runner.py --loop` process
    /// per stage, executing rounds in an overlapping wave schedule so adjacent
    /// rounds run concurrently across stages.
    /// Equivalent to `StreamPipeline` but for Python workloads.
    PyPipeline(PyPipelineParams),
    /// Send all records from a local SHM slot to a remote peer over the RDMA
    /// control channel (TCP).  Requires `dag.rdma` to be configured.
    ///
    /// ```json
    /// { "kind": { "RemoteSend": { "slot": 30, "slot_kind": "Stream", "peer": 1 } } }
    /// ```
    RemoteSend(RemoteSendParams),
    /// Receive records from a remote peer and append them into a local SHM slot.
    /// Blocks until the peer's `RemoteSend` has completed.  Requires `dag.rdma`.
    ///
    /// ```json
    /// { "kind": { "RemoteRecv": { "slot": 30, "slot_kind": "Stream", "peer": 0 } } }
    /// ```
    RemoteRecv(RemoteRecvParams),
    /// RDMA Fetch-and-Add on a named atomic on a specific remote peer (owner-pinned).
    ///
    /// Atomically adds `add` to the named atomic on `peer`'s SHM and stores the
    /// old value into the same named atomic in the LOCAL SHM.  Safe for any number
    /// of concurrent callers targeting the same owner.
    ///
    /// ```json
    /// { "kind": { "RemoteAtomicFetchAdd": { "peer": 1, "name": "counter", "add": 1 } } }
    /// ```
    RemoteAtomicFetchAdd(RemoteAtomicFetchAddParams),
    /// RDMA Compare-and-Swap on a named atomic on a specific remote peer (owner-pinned).
    ///
    /// If the named atomic on `peer`'s SHM equals `compare`, atomically replaces it
    /// with `swap`.  Stores the old (pre-swap) value into the local SHM atomic.
    ///
    /// ```json
    /// { "kind": { "RemoteAtomicCmpSwap": { "peer": 1, "name": "lock", "compare": 0, "swap": 1 } } }
    /// ```
    RemoteAtomicCmpSwap(RemoteAtomicCmpSwapParams),
    /// Push the local value of a named atomic to a remote peer via RDMA FAA (reduce step).
    ///
    /// Reads the local value of `name` then adds it to `peer`'s copy of the same
    /// atomic.  Used in the replicated-local + reduce pattern: each machine calls
    /// `RemoteAtomicPush` to contribute its partial result to an accumulator node,
    /// which ends up holding the global sum without any coordination overhead.
    ///
    /// ```json
    /// { "kind": { "RemoteAtomicPush": { "peer": 0, "name": "word_count" } } }
    /// ```
    RemoteAtomicPush(RemoteAtomicPushParams),
}

#[derive(Debug, Deserialize)]
pub struct WasmCallParams {
    pub func: String,
    pub arg: u32,
}

#[derive(Debug, Deserialize)]
pub struct PyFuncParams {
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
    pub policy: ShufflePolicy,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ShufflePolicy {
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
pub struct StreamPipelineStage {
    /// Exported WASM function name.
    pub func: String,
    /// First argument (typically the input slot).
    pub arg0: u32,
    /// Second argument.  `null` / `None` → inject the current round number.
    pub arg1: Option<u32>,
}

/// RDMA recv source configuration for a pipeline node.
///
/// Before each round of stage 0, a batch is received from `peer` into `slot`
/// using the already-established mesh connection.  The connection is reused
/// across all rounds — no reconnection happens between ticks.
///
/// The previous round's page chain in `slot` is freed automatically before
/// each receive (except round 0) to avoid leaking SHM pages.
#[derive(Debug, Deserialize)]
pub struct RdmaPipelineRecv {
    /// Mesh peer index to receive from.
    pub peer: usize,
    /// Local SHM slot index to populate before each round.
    pub slot: usize,
    /// Whether `slot` is in the Stream or I/O area.
    pub slot_kind: RemoteSlotKind,
    /// Handshake protocol — must match the paired `RemoteSend` on the peer.
    #[serde(default)]
    pub protocol: RemoteProtocol,
}

/// RDMA send sink configuration for a pipeline node.
///
/// After the last stage completes each round, `slot` is sent to `peer`
/// using the already-established mesh connection.  The connection is reused
/// across all rounds — no reconnection happens between ticks.
#[derive(Debug, Deserialize)]
pub struct RdmaPipelineSend {
    /// Mesh peer index to send to.
    pub peer: usize,
    /// Local SHM slot index to read and send after each completed round.
    pub slot: usize,
    /// Whether `slot` is in the Stream or I/O area.
    pub slot_kind: RemoteSlotKind,
    /// Handshake protocol — must match the paired `RemoteRecv` on the peer.
    #[serde(default)]
    pub protocol: RemoteProtocol,
    /// If `true`, free `slot` immediately after each send so the next round's
    /// stage starts with an empty chain.  Required when a pipeline stage always
    /// appends to the same fixed output slot (e.g. `img_load_ppm` writing to
    /// slot 20 every round) — without this, the slot accumulates every round's
    /// output and the RDMA write would include all previous rounds' data.
    ///
    /// Not needed when the stage uses the round number as its output slot
    /// (i.e. `arg1 = null` in `StreamPipelineStage`).
    #[serde(default)]
    pub free_after: bool,
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
///
/// To feed stage 0 from a remote peer instead of a locally-produced slot,
/// add `"rdma_recv"`.  To send the last stage's output to a remote peer
/// after each round, add `"rdma_send"`.  Both reuse the mesh connection
/// established at DAG start — the RDMA bridge is kept alive for all rounds.
#[derive(Debug, Deserialize)]
pub struct StreamPipelineParams {
    /// Number of rounds to execute.
    pub rounds: u32,
    /// Ordered list of pipeline stages.  Must contain at least one entry.
    pub stages: Vec<StreamPipelineStage>,
    /// Optional RDMA receive source: before each round, fetch a batch from a
    /// remote peer into a local slot so stage 0 can consume it.
    #[serde(default)]
    pub rdma_recv: Option<RdmaPipelineRecv>,
    /// Optional RDMA send sink: after the last stage completes each round,
    /// send a local slot's contents to a remote peer.
    #[serde(default)]
    pub rdma_send: Option<RdmaPipelineSend>,
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
pub enum FileDispatchPolicy {
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
    pub policy: FileDispatchPolicy,
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
    /// List of file paths.  Behaviour depends on `binary` and `cycle`:
    ///
    /// - `binary: false` (default): one path per run, cycling via
    ///   `paths[run % paths.len()]` (same as the single-`path` field).
    /// - `binary: true, cycle: false` (default): ALL paths loaded every run as
    ///   individual binary records; guests consume them with `read_next_io_record`.
    ///   Use this when a pipeline or grouping worker needs all records pre-loaded
    ///   and advances a cursor across rounds/nodes (e.g. `py_img_pipeline_demo`).
    /// - `binary: true, cycle: true`: ONE path per run, cycling via
    ///   `paths[run % paths.len()]`, loaded as a single binary record.
    ///   Use this when a single grouping node should process a different file each
    ///   run (e.g. `img_grouping_demo`).
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
    /// line-by-line.  Combine with `cycle` to control whether all paths or just
    /// one path per run is loaded.  See `paths` for the full matrix.
    #[serde(default)]
    pub binary: bool,
    /// Only meaningful when `binary: true` and `paths` has multiple entries.
    /// When `false` (default) all paths are loaded every run.
    /// When `true` one path is loaded per run, cycling via `paths[run % paths.len()]`.
    #[serde(default)]
    pub cycle: bool,
}

/// One stage in a `WasmGrouping`.
#[derive(Debug, Deserialize)]
pub struct WasmGroupingStage {
    /// Exported WASM function name.
    pub func: String,
    /// First argument (e.g. input slot).
    pub arg0: u32,
    /// Second argument (e.g. output slot).
    pub arg1: u32,
}

/// Parameters for the `WasmGrouping` node.
///
/// One persistent `wasm-loop` subprocess is spawned per stage (paying the
/// wasmtime JIT cost once per stage rather than once per call).  Stages are
/// called sequentially — no pipelining across rounds.
///
/// Example:
/// ```json
/// { "stages": [
///     { "func": "pipeline_source",    "arg0": 10, "arg1": 0  },
///     { "func": "pipeline_transform", "arg0": 10, "arg1": 20 },
///     { "func": "pipeline_sink",      "arg0": 20, "arg1": 0  }
/// ]}
/// ```
#[derive(Debug, Deserialize)]
pub struct WasmGroupingParams {
    /// Ordered list of stages.  Must contain at least one entry.
    pub stages: Vec<WasmGroupingStage>,
}

/// One stage in a `PyGrouping`.
#[derive(Debug, Deserialize)]
pub struct PyGroupingStage {
    /// Python workload function name (must exist in `workloads.py`).
    pub func: String,
    /// First argument passed to the function (default 0).
    #[serde(default)]
    pub arg: u32,
    /// Optional second argument.
    #[serde(default)]
    pub arg2: Option<u32>,
}

/// Parameters for the `PyGrouping` node.
///
/// A single persistent `runner.py --loop` process handles every stage call in
/// order.  This avoids paying the Python startup cost more than once.
/// Stages run strictly sequentially — no pipelining across rounds.
///
/// Example (image processing grouping):
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
pub struct PyGroupingParams {
    /// Ordered list of stages.  Must contain at least one entry.
    pub stages: Vec<PyGroupingStage>,
}

/// One stage in a true `PyPipeline`.
///
/// When `arg2` is `None` the host injects the current round number in its place,
/// following the same convention as `StreamPipelineStage` in `StreamPipeline`.
#[derive(Debug, Deserialize)]
pub struct PyStreamPipelineStage {
    /// Python workload function name (must exist in `workloads.py`).
    pub func: String,
    /// First argument (typically the input slot, default 0).
    #[serde(default)]
    pub arg: u32,
    /// Second argument.  `null` / `None` → inject the current round number.
    #[serde(default)]
    pub arg2: Option<u32>,
}

/// Parameters for the `PyPipeline` node.
///
/// Stages are executed in a pipelined wave schedule so adjacent rounds overlap.
/// For N rounds and D stages the total ticks = N + D − 1.
/// One persistent `runner.py --loop` process is spawned per stage; within each
/// tick all active stage workers are scatter-sent first, then gathered — giving
/// intra-tick concurrency across stages.
///
/// Example (4-stage Python pipeline):
/// ```json
/// { "rounds": 8, "stages": [
///     { "func": "py_source",    "arg": 10, "arg2": null },
///     { "func": "py_filter",    "arg": 10, "arg2": 20   },
///     { "func": "py_transform", "arg": 20, "arg2": 30   },
///     { "func": "py_sink",      "arg": 30, "arg2": 40   }
/// ]}
/// ```
///
/// Like `StreamPipeline`, `"rdma_recv"` / `"rdma_send"` keep the RDMA bridge
/// alive across all rounds and integrate recv/send into each pipeline tick.
#[derive(Debug, Deserialize)]
pub struct PyPipelineParams {
    /// Number of rounds to execute.
    pub rounds: u32,
    /// Ordered list of pipeline stages.  Must contain at least one entry.
    pub stages: Vec<PyStreamPipelineStage>,
    /// Optional RDMA receive source: before each round, fetch a batch from a
    /// remote peer into a local slot so stage 0 can consume it.
    #[serde(default)]
    pub rdma_recv: Option<RdmaPipelineRecv>,
    /// Optional RDMA send sink: after the last stage completes each round,
    /// send a local slot's contents to a remote peer.
    #[serde(default)]
    pub rdma_send: Option<RdmaPipelineSend>,
}

/// Selects the TCP/RDMA handshake protocol for a `RemoteSend` / `RemoteRecv` pair.
///
/// - `SenderInit` (default): sender announces `total_bytes` first, receiver
///   replies with `dest_off`, sender writes and signals done.  Safe for any
///   unidirectional transfer.
/// - `ReceiverInit`: receiver announces `(dest_off, avail_cap)` first, sender
///   writes then sends `total_bytes` as the done signal, receiver structures
///   the page chain in-place.  Avoids deadlock in bidirectional (shuffle)
///   scenarios because both sides send their announcement without waiting.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProtocol {
    #[default]
    SenderInit,
    ReceiverInit,
}

/// Which SHM slot area a `RemoteSend` / `RemoteRecv` node operates on.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum RemoteSlotKind {
    /// Stream slot area (`writer_heads` / `writer_tails`).
    Stream,
    /// I/O slot area (`io_heads` / `io_tails`).
    Io,
}

/// Parameters for the `RemoteSend` node.
#[derive(Debug, Deserialize)]
pub struct RemoteSendParams {
    /// Local SHM slot index to read records from.
    pub slot: usize,
    /// Whether the slot is in the Stream or Io area.
    pub slot_kind: RemoteSlotKind,
    /// Mesh peer index to send records to.
    pub peer: usize,
    /// Handshake protocol.  Defaults to `SenderInit`; use `ReceiverInit`
    /// for bidirectional (shuffle) transfers to avoid deadlock.
    #[serde(default)]
    pub protocol: RemoteProtocol,
}

/// Parameters for the `RemoteRecv` node.
#[derive(Debug, Deserialize)]
pub struct RemoteRecvParams {
    /// Local SHM slot index to write received records into.
    pub slot: usize,
    /// Whether the slot is in the Stream or Io area.
    pub slot_kind: RemoteSlotKind,
    /// Mesh peer index to receive records from.
    pub peer: usize,
    /// Handshake protocol.  Must match the paired `RemoteSend` node.
    #[serde(default)]
    pub protocol: RemoteProtocol,
}

/// Parameters for `RemoteAtomicFetchAdd`.
#[derive(Debug, Deserialize)]
pub struct RemoteAtomicFetchAddParams {
    /// Mesh peer whose SHM atomic is the target.
    pub peer: usize,
    /// Registered name of the `AtomicU64` in both peers' atomic arenas.
    pub name: String,
    /// Value to add to the remote atomic.
    pub add:  u64,
}

/// Parameters for `RemoteAtomicCmpSwap`.
#[derive(Debug, Deserialize)]
pub struct RemoteAtomicCmpSwapParams {
    /// Mesh peer whose SHM atomic is the target.
    pub peer:    usize,
    /// Registered name of the `AtomicU64` in both peers' atomic arenas.
    pub name:    String,
    /// Expected current value; the swap only fires when the remote equals this.
    pub compare: u64,
    /// Value to write if the compare succeeds.
    pub swap:    u64,
}

/// Parameters for `RemoteAtomicPush`.
#[derive(Debug, Deserialize)]
pub struct RemoteAtomicPushParams {
    /// Mesh peer to accumulate into (the "owner" / reduce target).
    pub peer: usize,
    /// Registered name of the `AtomicU64`.  Must exist in both SHMs.
    pub name: String,
}
