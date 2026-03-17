# Updates

Changes made since the initial README, in order.

---

## 1. PyFunc parallel wave execution

**Files:** `host/src/runtime/dag_runner/plan.rs`, `subprocess.rs`, `executor.rs`

`PyFunc` nodes were previously treated as host-executed nodes and ran **sequentially and blocking** inside `execute_node()` via `.status()`. They are now classified as subprocess nodes alongside `WasmVoid` / `WasmU32` / `WasmFatPtr`.

- `is_subprocess_node()` (renamed from `is_subprocess_wasm_node`) now includes `NodeKind::PyFunc`.
- A new `spawn_python_subprocess()` function extracts the `python3` / `wasmtime run` command building from `execute_node` and returns a `Child` handle instead of blocking.
- The wave executor (step 3a) dispatches to `spawn_python_subprocess` for `PyFunc` nodes and to `spawn_wasm_subprocess` for WASM nodes. All active subprocess nodes in a wave are spawned first, then waited on together — giving intra-wave parallelism to Python nodes the same way WASM nodes already had it.
- The `execute_node` `PyFunc` branch is retained as a sequential fallback (spawn + wait) but is no longer reached in normal wave execution.

---

## 2. StreamPipeline persistent worker reuse

**Files:** `host/src/main.rs`, `host/src/runtime/worker.rs`, `host/src/runtime/dag_runner/subprocess.rs`, `executor.rs`

Previously, each `StreamPipeline` tick spawned a fresh `./host wasm-call` subprocess per active stage, paying the full wasmtime JIT + SHM mmap cost on every tick.

### New `wasm-loop` subcommand

```
./host wasm-loop <shm_path> <wasm_path> <func>
```

`run_wasm_loop()` in `worker.rs` sets up wasmtime and the SHM mapping **once**, then enters a blocking stdin loop:

- Reads `"arg0 arg1\n"` from stdin (OS blocks the process between reads — no busy-waiting).
- Calls `func(arg0, arg1)`.
- Writes `"ok\n"` (or `"err: …\n"`) to stdout and flushes.
- Exits when stdin closes (EOF).

### `PipelineWorker` struct

`subprocess.rs` now contains `PipelineWorker`, which wraps a `wasm-loop` child process with typed `send(arg0, arg1)` and `recv()` methods and a `finish()` shutdown path. `Drop` closes stdin before waiting so the worker exits cleanly even on error paths.

### Scatter / gather tick dispatch

The `StreamPipeline` executor in `executor.rs`:

1. **Before the tick loop** — spawns one `PipelineWorker` per stage (wasmtime init paid once per pipeline execution).
2. **Each tick — scatter** — writes commands to all active stage workers' stdin pipes in order. Workers wake up and execute concurrently (OS-level parallelism via the pipe wait queues).
3. **Each tick — gather** — reads responses from each active stage in order.
4. **After the tick loop** — calls `finish()` on all workers (closes stdin → EOF → processes exit).

Cost comparison for a 6-stage pipeline over 3 reset-mode runs:

| | Before | After |
|--|--|--|
| Subprocess spawns | `ticks × active_stages × runs` | `stages × runs` |
| wasmtime JIT per stage | every tick | once per run |

---

## 3. Python DAG JSON corrections

**Files:** `demo_dag/py_img_pipeline_demo.json`, `demo_dag/py_word_count_demo.json`

### `py_img_pipeline_demo.json`
- `Output` node: added explicit `"slot": 1` (OUTPUT_IO_SLOT) for clarity, matching the Rust version.
- `FreeSlots` node: `"io": [10]` → `"io": [10, 1]` to also release the output I/O slot between reset runs.

### `py_word_count_demo.json`
- `Output` node: added explicit `"slot": 1` to match the Rust `word_count_demo.json` convention.

---

## 4. `dag_runner.rs` split into module directory

**Files:** `host/src/runtime/dag_runner/` (new directory)

The 1538-line `dag_runner.rs` was split into five focused files. `runtime/mod.rs` is unchanged — `pub mod dag_runner;` resolves to `dag_runner/mod.rs` automatically.

| File | Lines | Contents |
|------|-------|----------|
| `mod.rs` | ~424 | Module doc, `run_dag_file`, `run_dag_json`, `run_dag` |
| `types.rs` | ~321 | All JSON schema structs and enums |
| `plan.rs` | ~244 | `validate_dag`, `topo_sort`, `build_waves`, slot tracking, `is_subprocess_node` |
| `subprocess.rs` | ~164 | `PipelineWorker`, `spawn_wasm_subprocess`, `spawn_python_subprocess` |
| `executor.rs` | ~413 | `execute_node` and all `NodeKind` match arms |

---

## 5. `dag_demo.json` — StreamPipeline schema update

**File:** `demo_dag/dag_demo.json`

The `stream_pipeline_demo` node used a flat schema (`source_slot`, `filter_slot`, `transform_slot`, `summary_slot`) that no longer matches `StreamPipelineParams`. Updated to the current `stages` array format:

```json
"StreamPipeline": {
  "rounds": 5,
  "stages": [
    { "func": "pipeline_source",    "arg0": 200, "arg1": null },
    { "func": "pipeline_filter",    "arg0": 200, "arg1": 201 },
    { "func": "pipeline_transform", "arg0": 201, "arg1": 202 },
    { "func": "pipeline_sink",      "arg0": 202, "arg1": 203 }
  ]
}
```

`arg1: null` on `pipeline_source` tells the executor to inject the current round number as the second argument.

---

## 6. `PyPipeline` node kind

**Files:** `types.rs`, `subprocess.rs`, `executor.rs`, `py_guest/python/runner.py`, `demo_dag/py_img_pipeline_demo.json`

Adds the Python equivalent of `StreamPipeline` worker reuse.

### `runner.py --loop` mode

When invoked with `--loop`, `runner.py` stays alive reading `"func [arg [arg2]]\n"` from stdin, dispatching to `workloads`, writing `"ok\n"` or `"err: …\n"` back, then blocking again. The original one-shot env-var mode is unchanged (no `--loop` argument).

### New types

```
PyPipelineStage  { func: String, arg: u32, arg2: Option<u32> }
PyPipelineParams { stages: Vec<PyPipelineStage> }
NodeKind::PyPipeline(PyPipelineParams)
```

### `PyPipelineWorker`

One persistent `runner.py --loop` process per `PyPipeline` node (not one per stage — Python stages are sequential with no intra-tick overlap). Supports both `python3` and `wasmtime run python.wasm` backends. `call(func, arg, arg2)` writes the command and reads the response synchronously. `finish()` closes stdin → EOF → process exits.

### `py_img_pipeline_demo.json`

The six individual `PyFunc` nodes (`parse`, `rotate`, `gray`, `equalize`, `blur`, `export`) are replaced by a single `PyPipeline` node:

```json
{
  "id": "pipeline",
  "deps": ["load"],
  "kind": { "PyPipeline": {
    "stages": [
      { "func": "img_load_ppm"   },
      { "func": "img_rotate"     },
      { "func": "img_grayscale"  },
      { "func": "img_equalize"   },
      { "func": "img_blur"       },
      { "func": "img_export_ppm" }
    ]
  }}
}
```

Cost comparison (3 reset-mode runs):

| | Before (6 × PyFunc) | After (1 × PyPipeline) |
|--|--|--|
| Process spawns per run | 6 | 1 |
| Process spawns for 3 runs | 18 | 3 |
| Python / wasmtime startup cost | paid 6× per run | paid 1× per run |

The `save` node's `deps` is updated from `["export"]` to `["pipeline"]`.

---

## 7. `input_output/` module directory

**Files:** `host/src/runtime/input_output/` (new directory)

`inputer.rs`, `outputer.rs`, and `logger.rs` were moved from `host/src/runtime/` into a new `input_output/` subdirectory. A `mod.rs` was added to declare the three submodules. `runtime/mod.rs` now declares `pub mod input_output;` in place of the three individual entries.

| Old path | New path |
|---|---|
| `runtime/inputer.rs` | `runtime/input_output/inputer.rs` |
| `runtime/outputer.rs` | `runtime/input_output/outputer.rs` |
| `runtime/logger.rs` | `runtime/input_output/logger.rs` |

All `use crate::runtime::{inputer, outputer, logger}` references across `slicer.rs`, `dag_runner/plan.rs`, `dag_runner/mod.rs`, and `dag_runner/executor.rs` were updated to `crate::runtime::input_output::{inputer, outputer, logger}`.

The only internal path fix was in `inputer.rs`: `use super::reclaimer` → `use crate::runtime::reclaimer` (now `crate::runtime::mem_operation::reclaimer` after update 9), and in `outputer.rs`: `use super::writer::read_io_records` → `use crate::runtime::writer::read_io_records` (now `use super::state_writer::read_io_records` after update 8).

---

## 8. `writer.rs` → `input_output/state_writer.rs`

**Files:** `host/src/runtime/input_output/state_writer.rs` (renamed and moved)

`writer.rs` was renamed to `state_writer.rs` and moved into `input_output/`. The module is declared in `input_output/mod.rs` alongside the other three submodules. `runtime/mod.rs` no longer declares `pub mod writer;`.

Import sites updated:

| File | Old import | New import |
|---|---|---|
| `input_output/outputer.rs` | `crate::runtime::writer::read_io_records` | `super::state_writer::read_io_records` |
| `dag_runner/executor.rs` | `crate::runtime::writer::{PersistenceOptions, PersistenceWriter}` | `crate::runtime::input_output::state_writer::{…}` |
| `dag_runner/mod.rs` | `crate::runtime::writer::PersistenceWriter` | `crate::runtime::input_output::state_writer::PersistenceWriter` |

---

## 9. `mem_operation/` module directory

**Files:** `host/src/runtime/mem_operation/` (new directory)

`reclaimer.rs` and `slicer.rs` were moved from `host/src/runtime/` into a new `mem_operation/` subdirectory. `runtime/mod.rs` now declares `pub mod mem_operation;` in place of the two individual entries.

| Old path | New path |
|---|---|
| `runtime/reclaimer.rs` | `runtime/mem_operation/reclaimer.rs` |
| `runtime/slicer.rs` | `runtime/mem_operation/slicer.rs` |

All references updated across `input_output/inputer.rs`, `runtime/organizer.rs`, `dag_runner/plan.rs`, `dag_runner/mod.rs`, `dag_runner/executor.rs`, and `routing/dispatch.rs`.

---

## 10. `connect` crate — RDMA state-sharing demo

**Files:** `connect/` (new workspace crate)

A new `connect` crate was added to the workspace for RDMA-based state sharing between two nodes.  The existing placeholder `connect/remote.rs` was moved to `connect/src/remote.rs` and filled in as the high-level API.

### Why a custom FFI instead of `rdma-sys`

`rdma-sys 0.3.0` uses `bindgen 0.59.2`, which panics on an anonymous union introduced in the `ib_user_ioctl_verbs.h` header of `rdma-core 50.0` (the version installed on this machine).  Rather than forking the crate, all libibverbs types and function declarations are hand-written in `src/ffi.rs` with layouts verified against the installed headers using `offsetof`/`sizeof` probes.

`ibv_post_send`, `ibv_poll_cq`, `ibv_post_recv`, and `ibv_query_port` are `static inline` in the libibverbs headers and therefore not exported symbols.  `src/ibverbs_helpers.c` (compiled via the `cc` build dependency) re-exports them as real C symbols callable from Rust FFI.

### File layout

```
connect/
  build.rs                    link libibverbs + compile ibverbs_helpers.c
  src/
    ibverbs_helpers.c         C wrappers for static-inline ibv_ functions
    ffi.rs                    hand-written FFI types + extern "C" declarations
    lib.rs
    remote.rs                 RdmaRemote — high-level two-node API
    rdma/
      mod.rs
      context.rs              RdmaContext  (ibv_context + ibv_pd + ibv_cq)
      memory_region.rs        MemoryRegion (registered buffer, lkey/rkey)
      queue_pair.rs           QueuePair    (RESET→INIT→RTR→RTS, post_rdma_write)
      exchange.rs             TCP side-channel for QpInfo swap + done signal
  examples/
    rdma_server.rs
    rdma_client.rs
```

### QP state machine

`QueuePair` drives the standard RC transition sequence:

| Step | ibv call | Key fields set |
|---|---|---|
| `to_init(port)` | `ibv_modify_qp` | `IBV_QPS_INIT`, access flags, port |
| `to_rtr(remote, port, gid_idx)` | `ibv_modify_qp` | `IBV_QPS_RTR`, peer QPN/PSN, AH attr (GID for RoCE) |
| `to_rts(psn)` | `ibv_modify_qp` | `IBV_QPS_RTS`, local PSN, timeout/retry |

### Data path

`RdmaRemote::write_state(data)` (client side):
1. Copies `data` into the local registered MR.
2. Posts `IBV_WR_RDMA_WRITE | IBV_SEND_SIGNALED` — the HCA DMA-writes directly into the server's MR with no server CPU involvement.
3. Busy-polls the local CQ for the write completion.
4. Sends a one-byte TCP signal so the server knows to read its buffer.

`RdmaRemote::wait_peer_write()` (server side): blocks on the TCP control channel for that signal, then the application reads `local_state()`.

### Running the demo

```bash
# terminal 1 — start server first
cargo run --example rdma_server

# terminal 2
cargo run --example rdma_client           # loopback
cargo run --example rdma_client -- <ip>   # cross-node
```

The machine has an active `mlx4_0` port 1 (RoCE, netdev `eno1`), so the demo runs without any additional hardware configuration.
