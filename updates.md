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
