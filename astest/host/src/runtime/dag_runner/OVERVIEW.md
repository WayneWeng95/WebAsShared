# dag_runner

DAG-based execution engine for WebAsShared stream-processing workloads.

A DAG is a JSON file describing nodes and their dependencies.  The runner
topologically sorts the nodes, groups them into parallel *waves*, and executes
each wave — spawning subprocesses for WASM/Python one-shot nodes while running
host-side routing and pipeline nodes on the main thread.

---

## File structure

```
dag_runner/
├── types.rs      — JSON schema: all Dag / DagNode / NodeKind structs
├── plan.rs       — DAG validation, topo-sort, wave building, slot-lifetime tracking
├── workers.rs    — Persistent loop-worker processes + one-shot spawn helpers
├── grouping.rs   — Sequential multi-stage execution (WasmGrouping, PyGrouping)
├── pipeline.rs   — Pipelined wave execution (StreamPipeline, PyPipeline)
├── dispatch.rs   — Single-node dispatcher: routes each NodeKind to its handler
├── mod.rs        — Public entry points (run_dag, run_dag_file, run_dag_json)
└── OVERVIEW.md   — This file
```

---

## types.rs — JSON schema

Defines the complete set of types that are deserialized from the DAG JSON file.

### Top-level types

| Type | Role |
|---|---|
| `Dag` | Root struct: `shm_path`, `mode`, `runs`, `nodes`, Python/WASM paths, log level |
| `DagMode` | Enum: `OneShot` (run once) or `Reset` (loop until run limit / SIGINT) |
| `DagNode` | A single node: `id`, `deps` (dependency IDs), `kind` |
| `NodeKind` | Enum discriminating every node type (see below) |

### NodeKind variants and their params structs

| Variant | Params struct | Description |
|---|---|---|
| `WasmVoid` | `WasmCallParams` | Call `func(arg) → ()` as a one-shot WASM subprocess |
| `WasmU32` | `WasmCallParams` | Call `func(arg) → u32`, result logged |
| `WasmFatPtr` | `WasmCallParams` | Call `func(arg) → u64`, fat-pointer decoded and printed |
| `PyFunc` | `PyFuncParams` | Run a Python workload function as a one-shot subprocess |
| `Bridge` | `BridgeParams` | Zero-copy 1→1 stream redirect on the host |
| `Aggregate` | `AggregateParams` | N→1 merge of upstream slots into one downstream slot |
| `Shuffle` | `ShuffleParams` | N→M routing with a pluggable `ShufflePolicy` |
| `Persist` | `PersistParams` | Snapshot atomics / stream slots / shared state to disk |
| `Watch` | `WatchParams` | Lightweight single-slot or single-entry persist |
| `Input` | `InputParams` | Load a file into an I/O slot for guest consumption |
| `Output` | `OutputParams` | Drain an I/O slot to a file after the guest has written it |
| `FreeSlots` | `FreeSlotsParams` | Return stream/I/O slot page chains to the SHM pool and reset their atomic cursors |
| `FileDispatch` | `FileDispatchParams` | Load a file, slice it with a `FileDispatchPolicy`, dispatch to N workers |
| `OwnedDispatch` | `OwnedDispatchParams` | Dispatch inline byte payloads to N workers |
| `StreamPipeline` | `StreamPipelineParams` | Pipelined WASM execution across rounds (wave schedule) |
| `WasmGrouping` | `WasmGroupingParams` | Sequential WASM stages, one persistent worker per stage |
| `PyGrouping` | `PyGroupingParams` | Sequential Python stages, one shared persistent worker |
| `PyPipeline` | `PyPipelineParams` | Pipelined Python execution across rounds (wave schedule) |

### Stage types

| Type | Owner | Description |
|---|---|---|
| `StreamPipelineStage` | `StreamPipelineParams` | WASM func + `arg0` (input slot) + optional `arg1` (output slot; `None` → inject round number) |
| `WasmGroupingStage` | `WasmGroupingParams` | WASM func + `arg0` + `arg1` (both explicit) |
| `PyGroupingStage` | `PyGroupingParams` | Python func + `arg` + optional `arg2` |
| `PyPipelineStage` | `PyPipelineParams` | Python func + `arg` + optional `arg2` (`None` → inject round number) |

### Policy enums

| Type | Used by | Description |
|---|---|---|
| `ShufflePolicy` | `ShuffleParams` | `Modulo`, `RoundRobin`, `FixedMap`, `Broadcast` |
| `FileDispatchPolicy` | `FileDispatchParams` | `Equal`, `LineBoundary`, `FixedSize { max_bytes }` |

### Input loading modes (`InputParams`)

`binary` and `cycle` together determine how paths are loaded each run:

| `binary` | `cycle` | Behaviour |
|---|---|---|
| `false` | — | One path per run, cycling via `paths[run % len]` (line-by-line records) |
| `true` | `false` | All paths loaded every run as individual binary records |
| `true` | `true` | One path per run (cycling), loaded as a single binary record |

---

## plan.rs — Planning utilities

Static analysis and planning helpers used by `mod.rs` before the execution loop begins.

| Function | Role |
|---|---|
| `validate_dag(dag)` | Checks all declared slot IDs are within `STREAM_SLOT_COUNT` / `IO_SLOT_COUNT` bounds |
| `topo_sort(nodes)` | Kahn's algorithm; returns node indices in dependency order, errors on cycles |
| `build_waves(nodes, order)` | Groups the sorted indices into *waves* — sets of nodes with no intra-set dependencies that can run concurrently |
| `build_slot_refcounts(dag)` | Counts how many nodes read each exclusively-owned slot, used to know when it is safe to free |
| `node_owned_slots(kind)` | Returns the stream/I/O slots a node owns exclusively (freed when the last reader finishes) |
| `node_routed_upstream_slots(kind)` | Returns upstream stream slots whose pages have been transferred to a downstream chain via routing — only metadata needs clearing, not the pages |
| `is_oneshot_node(kind)` | Returns `true` for `WasmVoid/U32/FatPtr` and `PyFunc` — nodes that run as isolated fire-and-forget subprocesses (as opposed to loop-worker nodes) |
| `parse_level(s)` | Converts a log-level string (`"debug"`, `"info"`, …) to a `Level` for the `HostLogger` |

---

## workers.rs — Process management

Low-level process helpers with no DAG business logic.  Every subprocess type
in the system is spawned from here.

### One-shot spawn helpers

| Function | Description |
|---|---|
| `spawn_wasm_subprocess(node, shm_path, wasm_path)` | Spawns `host wasm-call <shm> <wasm> <func> <ret_type> <arg>` for `WasmVoid/U32/FatPtr` nodes; caller receives a `Child` and must `.wait()` |
| `spawn_python_subprocess(node, shm_path, script, wasm)` | Spawns `python3 <script>` (or `wasmtime run python.wasm -- <script>`) with env vars for `PyFunc` nodes; caller receives a `Child` and must `.wait()` |

### Persistent loop workers

Both worker types expose the same lifecycle: `spawn` → repeated send/recv → `finish`.

#### `WasmLoopWorker`

Wraps a persistent `host wasm-loop <shm> <wasm> <func>` subprocess.  The
process blocks on stdin between calls; the host wakes it by writing a command.

| Method | Description |
|---|---|
| `spawn(func, shm_path, wasm_path, node_id)` | Start the subprocess; JIT compilation happens once here |
| `send(arg0, arg1)` | Write `"arg0 arg1\n"` to stdin (non-blocking from host side) |
| `recv()` | Block until the worker writes `"ok\n"` back |
| `finish(self)` | Close stdin (EOF → process exits), then wait for it |

Used by both `execute_wasm_grouping` (sequential) and `execute_stream_pipeline` (scatter/gather).

#### `PyLoopWorker`

Wraps a persistent `python3 <script> --loop` (or equivalent WASM) subprocess.

| Method | Description |
|---|---|
| `spawn(shm_path, script, wasm, node_id)` | Start the subprocess; Python import happens once here |
| `call(func, arg, arg2)` | Blocking send + recv in one call; used by grouping |
| `call_async(func, arg, arg2)` | Send only — write `"func arg [arg2]\n"` to stdin without waiting |
| `recv()` | Block until the worker writes `"ok\n"` back |
| `finish(self)` | Close stdin (EOF → process exits), then wait for it |

`call_async` + `recv` together form the scatter/gather pair used by `execute_py_pipeline`.
Used by both `execute_py_grouping` (sequential via `call`) and `execute_py_pipeline` (scatter/gather).

---

## grouping.rs — Sequential multi-stage execution

No pipelining across rounds; each stage completes before the next begins.

| Function | Description |
|---|---|
| `execute_wasm_grouping(params, node_id, shm_path, wasm_path)` | Spawns one `WasmLoopWorker` per stage; calls each stage sequentially (`send` → `recv`) |
| `execute_py_grouping(params, node_id, shm_path, script, wasm)` | Spawns a single `PyLoopWorker` shared across all stages; calls each stage via `worker.call()` |

Both functions pay the subprocess startup cost once and amortise it across all stages,
unlike individual `WasmVoid` / `PyFunc` nodes which pay it per call.

---

## pipeline.rs — Pipelined wave execution

Overlapping wave schedule: stage S is active on tick T when `S ≤ T < rounds + S`.
Total ticks = `rounds + depth − 1` instead of `rounds × depth` for naïve serial execution.

```
tick 0:  [stage·0·r0]
tick 1:  [stage·0·r1]  [stage·1·r0]
tick 2:  [stage·0·r2]  [stage·1·r1]  [stage·2·r0]
...
```

Within each tick, active stages run concurrently via scatter/gather:
all commands are written first (scatter), then responses are collected in order (gather).

| Function | Description |
|---|---|
| `execute_stream_pipeline(params, node_id, shm_path, wasm_path)` | One `WasmLoopWorker` per stage; scatter/gather per tick |
| `execute_py_pipeline(params, node_id, shm_path, script, wasm)` | One `PyLoopWorker` per stage; scatter via `call_async`, gather via `recv` |

When a stage's `arg1` / `arg2` is `None`, the current round index is injected —
the convention for source stages that need to know which batch to produce.

---

## dispatch.rs — Node dispatcher

Single public function `execute_node` called by `mod.rs` for every host-side node
in each wave.  Its only job is to match on `NodeKind` and delegate:

- **Routing nodes** (`Bridge`, `Aggregate`, `Shuffle`) — executed inline via host stream APIs.
- **Utility nodes** (`Input`, `Output`, `FreeSlots`, `Watch`, `Persist`, `FileDispatch`, `OwnedDispatch`) — executed inline.
- **One-shot subprocess nodes** (`WasmVoid/U32/FatPtr`, `PyFunc`) — spawned via `workers::spawn_*` and waited on. In normal wave execution these are classified as one-shot nodes by `is_oneshot_node` and spawned in parallel by `mod.rs` *before* `execute_node` is called; the arm here is a sequential fallback.
- **Loop-worker nodes** (`StreamPipeline`, `WasmGrouping`, `PyPipeline`, `PyGrouping`) — delegated to `pipeline.rs` or `grouping.rs`.

---

## mod.rs — Public entry points and run loop

### Public API

| Function | Description |
|---|---|
| `run_dag_file(path)` | Read JSON from a file path and execute |
| `run_dag_json(json)` | Parse JSON from a string and execute |
| `run_dag(dag)` | Execute a pre-parsed `Dag` struct |

### Execution loop (`run_dag`)

1. **Validate** — `validate_dag` checks slot bounds.
2. **Format SHM** — fresh shared-memory region so no stale data leaks between runs.
3. **Setup** — create wasmtime engine, linker, WASM instance, optional `HostLogger`.
4. **Plan** — `topo_sort` → `build_waves` (computed once; reused every reset iteration).
5. **Per-wave execution** (repeated each run):
   - Pre-join any pending prefetch handles for nodes in this wave.
   - Partition wave into *one-shot* nodes (spawned in parallel) and *host* nodes (run on main thread).
   - Spawn all one-shot subprocesses; run all host nodes via `execute_node`; wait for subprocesses.
   - Post-wave slot reclamation: clear routed-upstream metadata, free exclusively-owned slots when their last reader finishes, free `StreamPipeline` internal slots, reclaim `Input` slots after all consumers complete.
6. **Reset loop** — if `mode == Reset`, repeat from step 5 until the run limit is reached or SIGINT.
