# Updates

Changes made since the initial README, in order.

---

## 1. PyFunc parallel wave execution

**Files:** `dag_runner/plan.rs`, `subprocess.rs`, `executor.rs`

`PyFunc` nodes were blocking-sequential inside `execute_node`. They are now classified as subprocess nodes alongside WASM kinds. `spawn_python_subprocess()` returns a `Child` handle; the wave executor spawns all subprocess nodes first then waits on them together, giving PyFunc the same intra-wave parallelism WASM nodes already had.

---

## 2. StreamPipeline persistent worker reuse

**Files:** `host/src/main.rs`, `worker.rs`, `subprocess.rs`, `executor.rs`

Added `wasm-loop` subcommand: sets up wasmtime and the SHM mapping once, then loops on stdin reading `"arg0 arg1\n"` → calls func → writes `"ok\n"`. `PipelineWorker` wraps a `wasm-loop` child with typed `send`/`recv`/`finish`. The `StreamPipeline` executor spawns one worker per stage before the tick loop (wasmtime JIT paid once), scatter-writes commands each tick, gather-reads responses, then shuts down after the loop.

| | Before | After |
|--|--|--|
| Subprocess spawns | `ticks × stages × runs` | `stages × runs` |
| wasmtime JIT per stage | every tick | once per run |

---

## 3. Python DAG JSON corrections

**Files:** `demo_dag/py_img_pipeline_demo.json`, `demo_dag/py_word_count_demo.json`

Added explicit `"slot": 1` on `Output` nodes; extended `FreeSlots` in `py_img_pipeline_demo.json` to also release the output I/O slot between reset runs.

---

## 4. `dag_runner.rs` split into module directory

**Files:** `host/src/runtime/dag_runner/` (new directory)

The 1538-line `dag_runner.rs` split into five files:

| File | Contents |
|------|----------|
| `mod.rs` | `run_dag_file`, `run_dag_json`, `run_dag` |
| `types.rs` | All JSON schema structs and enums |
| `plan.rs` | `validate_dag`, `topo_sort`, `build_waves`, slot tracking |
| `subprocess.rs` | `PipelineWorker`, `spawn_wasm_subprocess`, `spawn_python_subprocess` |
| `executor.rs` | `execute_node` and all `NodeKind` match arms |

---

## 5. `dag_demo.json` — StreamPipeline schema update

**File:** `demo_dag/dag_demo.json`

Updated `stream_pipeline_demo` from the old flat schema to the current `stages` array format. `arg1: null` on the source stage tells the executor to inject the current round number.

---

## 6. `PyPipeline` node kind

**Files:** `types.rs`, `subprocess.rs`, `executor.rs`, `runner.py`, `py_img_pipeline_demo.json`

Python equivalent of `StreamPipeline` worker reuse. `runner.py --loop` stays alive reading `"func [arg [arg2]]\n"` from stdin. `PyPipelineWorker` wraps one persistent process per `PyPipeline` node. The six individual `PyFunc` nodes in `py_img_pipeline_demo.json` are replaced by a single `PyPipeline` node — reducing process spawns from 18 to 3 across 3 reset-mode runs.

---

## 7. `input_output/` module directory

**Files:** `host/src/runtime/input_output/` (new directory)

`inputer.rs`, `outputer.rs`, and `logger.rs` moved into `input_output/`. All import sites updated.

| Old | New |
|-----|-----|
| `runtime/inputer.rs` | `runtime/input_output/inputer.rs` |
| `runtime/outputer.rs` | `runtime/input_output/outputer.rs` |
| `runtime/logger.rs` | `runtime/input_output/logger.rs` |

---

## 8. `writer.rs` → `input_output/state_writer.rs`

**File:** `host/src/runtime/input_output/state_writer.rs`

Renamed and moved into `input_output/`. Import sites in `outputer.rs`, `executor.rs`, and `dag_runner/mod.rs` updated accordingly.

---

## 9. `mem_operation/` module directory

**Files:** `host/src/runtime/mem_operation/` (new directory)

`reclaimer.rs` and `slicer.rs` moved into `mem_operation/`. All references updated across the dag runner, routing, and input_output modules.

| Old | New |
|-----|-----|
| `runtime/reclaimer.rs` | `runtime/mem_operation/reclaimer.rs` |
| `runtime/slicer.rs` | `runtime/mem_operation/slicer.rs` |

---

## 10. `connect` crate — initial RDMA two-node demo

**Files:** `connect/` (new workspace crate)

Hand-crafted libibverbs FFI (`src/ffi.rs`) instead of `rdma-sys`, which panics on the anonymous union in rdma-core 50.0. Static-inline ibv functions re-exported via `ibverbs_helpers.c`. Implements `RdmaContext`, `MemoryRegion`, `QueuePair` (RESET→INIT→RTR→RTS), and `RdmaRemote` for a basic two-node WRITE + TCP-signal demo.

---

## 11. Full-mesh RDMA, atomic ops, SHM integration, RemoteSend / RemoteRecv (Phase 1 & 2)

**Files:** `connect/src/mesh.rs`, `connect/src/rdma/queue_pair.rs`, `host/src/runtime/remote/mod.rs`, `host/src/runtime/dag_runner/{types,dispatch,mod}.rs`

### Full-mesh QP topology — `MeshNode`

`MeshNode::connect_all(node_id, total, ips)` establishes N×(N-1)/2 RC QP pairs.
Deadlock-free: lower-ID node acts as TCP server on `BASE_PORT + i*MAX_NODES + j`.
Data-path: `write_to`, `broadcast`, `wait_from`, `wait_all_writes`, `slot`, `slot_str`.

### RDMA atomic operations

Added to `QueuePair` and exposed on `MeshNode`:
- `fetch_and_add(peer_id, byte_offset, add_val)` — hardware-atomic FAA on a remote u64.
- `compare_and_swap(peer_id, byte_offset, compare_val, swap_val)` — hardware-atomic CAS.

Both target `remote_mr_base + byte_offset`, addressing any 8-byte-aligned location within the peer's MR.

### SHM as the RDMA Memory Region — `connect_all_on_shm`

Registers the mmap'd SHM as the single RDMA MR. `remote_mr_base + shm_offset` targets any byte in the peer's SHM directly; RDMA WRITEs land there with no intermediate copy.

### RemoteSend / RemoteRecv DAG nodes

New types: `RdmaConfig`, `RemoteSlotKind` (`Stream`/`Io`), `RemoteSendParams`, `RemoteRecvParams`, `NodeKind::RemoteSend/RemoteRecv`, `Dag.rdma: Option<RdmaConfig>`.

Staging area pre-allocated from the SHM bump allocator (`STAGE_PAGES_PER_PEER = 256`, 1 MiB per peer). Both machines format SHM identically then pre-allocate the same pages in the same order, so staging offsets are byte-for-byte identical everywhere:

`staging_offset(peer_id) = BUMP_ALLOCATOR_START + peer_id × STAGE_BYTES_PER_PEER`

**Phase 2 protocol:** sender serialises records into local staging → RDMA WRITE to peer's staging at the same offset → TCP 1-byte signal. Receiver waits on TCP, issues Acquire fence, deserialises records, appends to target slot.

`dispatch.rs` gains `mesh: Option<&mut MeshNode>` on `execute_node`; `mod.rs` sets up the mesh and passes it through every call. `host/Cargo.toml` gains `connect` dependency.

---

## 12. RDMA RemoteSend / RemoteRecv — Phase 3, zero-copy scatter-gather

**Files:** `connect/src/rdma/queue_pair.rs`, `connect/src/mesh.rs`, `host/src/runtime/remote/mod.rs`, `host/src/runtime/dag_runner/{types,mod,plan}.rs`

### `rdma.transfer` enable/disable switch

`RdmaConfig` gains `transfer: bool` (default `true`). When `false`, staging pre-allocation is skipped and `RemoteSend`/`RemoteRecv` nodes fail validation; the mesh still connects so RDMA atomics remain available.

### Eliminated serialisation — raw page-chain transfer

Replaced the `Vec<(u32, Vec<u8>)>` round-trip with a direct page-chain walk. Sender writes a 12-byte header (`u64` ready-counter = 0, `u32` total_bytes) into local staging then scatters source page data via RDMA (no CPU copy of payload). Receiver appends raw bytes from `staging[12+]` directly into the target slot — same byte format the WASM guest uses, no parsing.

**Staging layout:**
```
staging_offset(sender_id):
  [0..8]   u64  ready_counter  — 0 before write; RDMA FAA sets to 1
  [8..12]  u32  total_bytes    — byte count of raw page content
  [12+]         raw page data  — concatenated page.data[..cursor] bytes
```

### RDMA FAA primary signal, TCP backup

`rdma_write_staging` is now data-only. Two new methods on `MeshNode`:
- `rdma_signal_staging(peer_id, staging_offset)` — RDMA FAA increments peer's `staging[0..8]` counter (0 → 1); TCP `send_done` sent as backup.
- `wait_staging(peer_id, counter_ptr)` — spin-polls the u64 for up to 100 ms (Acquire fence on non-zero), then falls back to TCP `wait_done`.

### Zero-copy scatter-gather WRITE with automatic chunking

`max_send_sge` raised to `MAX_SEND_SGE` (16). `post_rdma_write_sge_list` gains `signaled: bool`.

`MeshNode::rdma_write_sge(peer_id, remote_off, sge_pairs: &[(u64, u32)])` accepts an unlimited number of SGE pairs. It chunks into slices of `MAX_SEND_SGE`, posts all but the last unsignaled, posts the last signaled, then polls the CQ once. RC QPs deliver completions in order, so the single CQ entry proves every preceding write landed at the remote.

`execute_remote_send` builds `sges = [(header_vaddr, 12), (page1.data, cursor1), …]` and calls `rdma_write_sge` — the HCA DMA-reads source pages directly into peer's staging. CPU touches only the 12-byte header.

### DAG validation

`validate_dag` now rejects: `RemoteSend`/`RemoteRecv` without `rdma.transfer: true`; `RemoteSend` with no dependencies (required to guarantee the slot producer finishes before the RDMA DMA reads from its pages).

---

## 13. RDMA pipelined send/recv — background threading in `StreamPipeline` / `PyPipeline`

**Files:** `host/src/runtime/dag_runner/pipeline.rs`

RDMA send and receive operations are now handled by background threads so stage processing is not blocked on network I/O.

### Pre-fetch recv

At the end of tick `r`, the recv thread for round `r+1` is spawned immediately: slot reset → `execute_remote_recv`. It is joined at the start of tick `r+1`, overlapping network wait with the stage computation of the current tick.

### Double-buffer send (`free_after: true`)

When the send config has `free_after: true`, the executor alternates between two staging slots each round:

```
buf_slot = rdma.slot + (round % 2)
```

The last stage's `arg1`/`arg2` is overridden at runtime to point to the current buffer slot so the WASM guest writes into the right staging area. The previous round's send thread is joined before spawning the next (TCP channel serialisation). At the end of the tick loop, any remaining `pending_send` is joined.

### Synchronous path

When `free_after: false`, send remains synchronous (no double-buffering needed).

Both `execute_stream_pipeline` and `execute_py_pipeline` use the same logic.

---

## 14. `remote/mod.rs` split into module directory

**Files:** `host/src/runtime/remote/` (new directory structure)

The single `remote/mod.rs` was split into five focused files:

| File | Contents |
|------|----------|
| `mod.rs` | Thin dispatcher: `execute_remote_send`, `execute_remote_recv`, `PAGE_DATA` constant |
| `sender_initiated.rs` | SI path: `send_si` (walk SGEs → RDMA write page-chain), `recv_si` (alloc pages → link slot) |
| `receiver_initiated.rs` | RI path: `send_ri` (recv dest_off → RDMA write flat), `recv_ri` (announce cap → recv bytes → build chain) |
| `shm.rs` | `collect_src_sges` (walk page chain → SGE list), `alloc_and_link`, `link_to_slot` |
| `rdma.rs` | `rdma_write_page_chain` (SI scatter across page boundaries), `rdma_write_flat` (RI chunked SGE write) |

An `OVERVIEW.md` documents both transfer protocols (SI/RI) with ASCII message-sequence diagrams and per-file function tables.

---

## 15. `connect/src/mesh.rs` split into module directory

**Files:** `connect/src/mesh/` (new directory structure)

The monolithic `mesh.rs` was split into five focused files:

| File | Contents |
|------|----------|
| `mod.rs` | Type definitions (`PeerLink`, `MeshNode`, `SendChannel`, `RecvChannel`, `AtomicChannel`), channel accessors, constants, `rand_psn` |
| `connect.rs` | `connect_all`, `connect_all_on_shm` — full-mesh TCP/QP bootstrap |
| `atomic.rs` | `AtomicChannel` (FAA, CAS), `MeshNode` SHM-targeted atomics, slot-level atomics |
| `data_path.rs` | `write_to`, `broadcast`, `wait_from`, `wait_all_writes`, `slot`, `slot_str`, `signal_peer`, `send_u32_to`, `recv_u32_from` |
| `staging.rs` | `rdma_write_sge`, `rdma_write_staging`, `rdma_signal_staging`, `wait_staging`, `rdma_faa_only`, `rdma_write_to_page_chain` |

Private fields on `PeerLink` and `MeshNode` use `pub(in crate::mesh)` so all child modules can access them without widening the crate-level API.

An `OVERVIEW.md` documents port-assignment rules, channel handle types, and per-file function tables.

---

## 16. RDMA demo DAGs moved to `rdma_demo_dag/`

**Files:** `rdma_demo_dag/` (new directory)

All RDMA-specific DAG JSON files moved out of `demo_dag/` into a dedicated `rdma_demo_dag/` directory:

| File | Purpose |
|------|---------|
| `rdma_word_count_node0/1.json` | WASM word count across two nodes |
| `rdma_py_word_count_node0/1.json` | Python word count across two nodes |
| `rdma_img_pipeline_node0/1.json` | WASM image pipeline across two nodes |
| `rdma_py_img_pipeline_node0/1.json` | Python image pipeline across two nodes |

`test_rdma_single_machine.sh` updated to reference `rdma_demo_dag/` instead of `demo_dag/`.

---

## 17. Guest API reference — `HELPER.md`

**Files:** `guest/src/api/HELPER.md`, `py_guest/python/HELPER.md`

Two API reference files document every public function available to workloads:

**Rust guest** (`guest/src/api/HELPER.md`): Input, Output, Stream slots (alloc/write/link/read), I/O slots (alloc/write/link/read), named atomics (get/set/CAS/FAA), shared state (read/write/clear), fan-out helpers, and utility functions.

**Python guest** (`py_guest/python/HELPER.md`): `read_input`, `write_output`, stream slot functions (`alloc_stream_slot`, `write_stream_slot`, `link_stream_slot`, `read_stream_slot`), I/O slot functions, cursor management (`read_cursor`/`write_cursor`), fan-out helpers (`count_records`/`fan_out_records`), and SHM constants. Notes WASI compatibility constraints and the key difference that Python cursors live in an in-process dict rather than SHM atomics.

---

## 18. RMMAP workloads — FINRA, ML Training, TF-IDF

**Files:** `Executor/guest/src/workloads/{finra.rs, ml_training.rs, tfidf.rs}`, `Executor/py_guest/python/{finra_workload.py, ml_workload.py, ai_workload.py}`, `DAGs/workload_dag/`, `DAGs/rdma_workload_dag/`

Implemented three representative workloads from the RMMAP paper (EuroSys'24, Section 5.1) in both Rust (no_std WASM) and Python (WASI):

**FINRA Audit** — Financial trade validation pipeline modeled after the FINRA serverless workflow. 4 stages: FetchPrivate || FetchPublic → 8 parallel audit rules (price outlier, large order, wash trade, spoofing, concentration, after-hours, penny stock, round lot) → MergeResults. Uses 50K synthetic trades. Writes only 2 summary records per rule to avoid non-atomic SHM allocator corruption from concurrent Python processes.

**ML Training** — Image classification pipeline modeled after the MNIST PCA + LightGBM workflow. 4 stages: Partition → PCA ×2 (power-iteration, 8 components) → Redistribute → Train ×8 decision stumps → Validate ensemble. Pure-Python/no_std implementation (no numpy/sklearn). Rust PCA uses f32 (WASM-native sqrt), avoids f64::ln (needs libm).

**TF-IDF** — Distributed feature extraction. 8 map workers compute per-shard TF/DF counts, reducer merges and computes TF-IDF scores (Rust uses floor_log2 integer approximation for IDF, Python uses math.log), outputs top-50 terms.

Each workload has single-node and RDMA DAG variants for both Rust and Python (8 DAG JSONs per workload × 3 workloads = 24 new DAG files).

---

## 19. `astest/` renamed to `Executor/`

**Files:** All files under `astest/` moved to `Executor/`.

The execution engine directory was renamed from `astest/` to `Executor/` to better reflect its role in the multi-machine architecture. All internal path references updated (test scripts, README).

---

## 20. DAG folders moved to `DAGs/`

**Files:** `DAGs/demo_dag/`, `DAGs/workload_dag/`, `DAGs/rdma_demo_dag/`, `DAGs/rdma_workload_dag/`

All four DAG JSON directories moved from `Executor/` to a top-level `DAGs/` folder alongside `Executor/` and `NodeAgent/`. All relative paths within DAG JSONs updated to resolve from the `WebAsShared/` project root (e.g., `../data/trades.csv` → `Executor/data/trades.csv`). Explicit `wasm_path` fields added to every DAG JSON since the compiled default (`../target/...`) no longer resolves from root.

---

## 21. NodeAgent — multi-machine deployment agent

**Files:** `NodeAgent/` (new workspace)

A Rust daemon that orchestrates distributed DAG execution across multiple machines. Runs alongside the Executor (which it spawns as a subprocess).

### Single-node mode (`run`)

```bash
./node-agent run DAGs/workload_dag/finra_demo.json
```

Spawns the Executor with live terminal output (stdout/stderr inherited), collects metrics (CPU, RSS, SHM bump offset) to `/tmp/node_agent_metrics.jsonl`, reports elapsed time on completion. No config file needed.

### Multi-node mode (`start` / `submit`)

Coordinator-worker model over TCP control plane (separate from the RDMA data plane):

1. **Coordinator** (node 0) listens for worker connections and job submissions.
2. **Workers** connect to the coordinator and wait for job assignments.
3. On `submit`, the coordinator parses a **ClusterDag** — a single JSON that describes the entire distributed workflow — splits it into per-node DAGs, injects RDMA config (node_id, total, ips) from the live cluster membership, and distributes them.
4. All workers launch their Executor simultaneously (critical for the RDMA mesh 200ms TCP bootstrap window).
5. Workers report `JobStarted`, periodic `Metrics`, and `JobCompleted`/`JobFailed` back to the coordinator.

### ClusterDag format

Combines what were previously separate `node0.json` / `node1.json` files into a single definition:

```json
{
  "shm_path_prefix": "/dev/shm/rdma_finra",
  "transfer": true,
  "node_dags": {
    "0": [ ... node 0 DAG nodes ... ],
    "1": [ ... node 1 DAG nodes ... ]
  }
}
```

Sample ClusterDags provided for word_count, finra, and ml_training in `NodeAgent/cluster_dags/`.

### Architecture

| Module | Purpose |
|--------|---------|
| `protocol.rs` | Length-prefixed JSON over TCP (4-byte BE length + JSON payload) |
| `config.rs` | `AgentConfig` from `agent.toml` (role, cluster IPs, paths, timeouts) |
| `cluster_dag.rs` | `ClusterDag` parsing and per-node splitting with RDMA injection |
| `executor.rs` | Spawn `host dag` subprocess, monitor via `try_wait()`, live or captured output |
| `metrics.rs` | CPU from `/proc/stat`, RSS from `/proc/self/status`, SHM bump offset from Superblock |
| `worker.rs` | Connect to coordinator with retry, receive jobs, launch executor, report status |
| `coordinator.rs` | Accept workers, distribute jobs, collect results, handle submit/status queries |

Design decisions: static membership (config file, no service discovery), TCP control plane (RDMA mesh is per-job inside the Executor), coordinator model (not P2P — matches existing asymmetric RDMA DAGs), separate process (Executor is single-run, NodeAgent is long-running).
