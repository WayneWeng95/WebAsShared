# Parallel Jobs & Job-Level Divide-and-Merge — design plan

**Status: PLAN (not yet implemented).** Two related capabilities:

- **Part A — multi-job parallelism:** run several *independent* jobs concurrently
  under one NodeAgent (each with its own SHM region), instead of one job at a time.
- **Part B — job-level divide-and-merge:** run a workload too large for a single
  WASM guest (the 4 GiB wasm32 address-space cap) by sharding it into several
  Part-A parallel jobs and merging their partial results in a **native (non-WASM)
  process**, which has no 4 GiB ceiling.

Part B is built on Part A, so Part A lands first.

---

## 0. What already works (so we don't rebuild it)

Each job's executor is a **separate `host` subprocess** that maps its **own** SHM
file (`{shm_path_prefix}_n{node_id}`) at `TARGET_OFFSET` into **its own** address
space. So two executors with *different* `shm_path_prefix` already coexist with
fully isolated SHM regions — there is no OS-level or wasm-address-space conflict
between concurrent SHM objects. **SHM multiplexing is not the blocker;** the
single-job assumptions in the NodeAgent and the RDMA mesh are.

---

## Part A — multiple parallel jobs

### A.1 The three blockers (all in NodeAgent / `connect`)

| # | Blocker | Where | Today | Needs |
|---|---------|-------|-------|-------|
| 1 | Worker tracks one job | `agent/src/worker.rs` | `current_executor: Option<ExecutorHandle>` (+ `current_shm_path`, `current_dag_json`); a 2nd `AssignJob` overwrites the 1st and orphans its process | `HashMap<JobId, JobCtx>` of concurrent executors; route `JobStarted`/`AbortJob`/completion per job |
| 2 | **RDMA mesh ports collide** | `connect/src/mesh/connect.rs` | `conn-1 = BASE_PORT + i*MAX_NODES + j` (+ BASE_PORT2/3/4); derived **only** from node ids — no job component | a per-job **slot** offset in the formula so two jobs on the same nodes use disjoint ports |
| 3 | Coordinator tracks one job | `agent/src/coordinator.rs` | `current_job_id: Option<String>` | a job map; metrics/status already loop per node, so they extend naturally |

Plus a soft constraint — **memory/core admission**: each executor's SHM window can
grow to ~3.48 GiB and each fans out up to `cores` worker subprocesses, so K parallel
jobs want ~K× both. The agent must refuse a job that won't fit.

### A.2 Central idea — a per-job **slot**

Introduce a small integer **job slot** `s ∈ [0, MAX_PARALLEL)` allocated by the
coordinator per admitted job. Everything that is currently global-per-node becomes
keyed by `s`:

- **SHM path:** `shm_path_prefix` → `"{prefix}_s{s}"` (then `_n{node}` as today).
  Guarantees disjoint SHM files even if two jobs share a base prefix.
- **Mesh ports:** reserve a fixed **port span** per slot. With `MAX_NODES` nodes the
  mesh uses `MAX_NODES²` ports per connection class; give each slot its own band:
  ```
  conn-k port = BASE_PORT_k + s * (MAX_NODES * MAX_NODES) + i*MAX_NODES + j
  ```
  `MAX_PARALLEL` slots × 4 conn classes × `MAX_NODES²` ports must stay inside the
  ephemeral range — size `MAX_PARALLEL` (e.g. 4) and `MAX_NODES` (e.g. 32)
  accordingly, or use a registered base well above the OS ephemeral range.
- **Metrics/job state:** keyed by job id (already string-keyed in messages).

The slot is the single knob that makes two jobs non-interfering; SHM isolation then
falls out for free.

### A.3 Component changes

**Coordinator (`coordinator.rs`)**
- `SlotAllocator`: a bitset of `MAX_PARALLEL` slots; `acquire()/release()`.
- `jobs: HashMap<JobId, JobState>` replacing the single `current_job_id`
  (`JobState { slot, nodes, started_at, per-node status }`).
- **Admission:** on submit, check (a) a free slot, (b) projected memory on each
  participating node (from the live `ScxClusterView` / `rss` + the job's declared
  SHM budget) vs a per-node cap. Reject with a clear error if it won't fit.
- Stamp the chosen `slot` into the split per-node DAGs (mesh section + shm prefix).
- Run staging + assignment + completion handling **per job** (the staging fix
  already sends each worker only its files, so concurrent stages don't clash on
  data; they only need disjoint mesh ports, handled by the slot).

**Mesh (`connect/src/mesh/connect.rs`, constants in `connect/src/.../lib.rs`)**
- Add `slot` to `RdmaSection`/mesh bring-up and to the four port formulas. This is
  the most surgical *real* code change. Everything downstream (QP exchange, conn-1..4)
  already takes a computed port; only the derivation changes.

**Worker (`worker.rs`)**
- `executors: HashMap<JobId, JobCtx>` where `JobCtx { handle, shm_path, dag_json, staged_files }`.
- `AssignJob` inserts; `AbortJob`/completion removes by job id; the poll loop reaps
  *all* finished executors, not just one.
- Per-job staged-file cleanup (the staging change already tracks per-job paths).

**Per-node DAG (`cluster_dag.rs` split)**
- `PerNodeDag.shm_path` already derives from the prefix — inject the slot there.
- Thread `slot` into the `RdmaSection`.

### A.4 Phasing
1. **Single-node, no-RDMA parallelism first** (lowest risk): slotting the SHM prefix
   + worker/coordinator job maps. Two `node-agent run`-style jobs on one box, no mesh.
2. **Mesh port-slotting** → two RDMA cluster jobs on the same node set in parallel.
3. **Admission control** (slots + memory) and clean rejection.
4. **Status/metrics** surfacing K concurrent jobs.

### A.5 Risks
- Mesh port-range exhaustion / overlap with OS ephemeral ports — pick bases + spans
  deliberately (see A.2) and assert non-overlap at startup.
- Orphan executors on coordinator crash (already a known robustness gap) gets worse
  with K jobs — reap by job id on reconnect.
- Memory: K jobs × up-to-3.48 GiB SHM — admission control is mandatory, not optional.

---

## Part B — job-level divide-and-merge (beyond the 4 GiB WASM cap)

**Goal:** process a workload whose working set exceeds one WASM guest's 4 GiB
wasm32 space by splitting it into shards, running each shard as a **Part-A parallel
job**, and combining the shard outputs in a **native process** (no wasm cap).

### B.1 Model
```
            ┌── shard job 0 (slot 0) ──► partial_0   ┐
 input ──►  ├── shard job 1 (slot 1) ──► partial_1   ├─► native merger ─► final
            ├── shard job 2 (slot 2) ──► partial_2   │   (standard process,
            └── shard job 3 (slot 3) ──► partial_3   ┘    > 4 GiB allowed)
```
- Each **shard job** is an ordinary ClusterDag (could itself be multi-node), run
  concurrently via Part A. It writes a **partial result** (its existing `Output`).
- The **merger** is a **native host-side process** (Rust, not a WASM guest): it
  reads the partial files / SHM regions and produces the final result. Because it's
  native it can hold > 4 GiB and use ordinary heap — exactly the case the user
  flagged where WASM can't.

### B.2 What this needs on top of Part A
- A **job-group** abstraction in the coordinator: `submit_group(shards[], merge_spec)`
  — admit shards as slots free up (back-pressure when `MAX_PARALLEL` is full),
  await all, then run the merge step. (A group is "done" only when the merge is.)
- A **merge step kind**: simplest is a coordinator-side **native merge command**
  (a separate small binary, or a `host merge` subcommand) that takes the shard
  output paths and a reducer name and writes the final file. Reuses the workload's
  own reduce logic re-implemented natively, *or* — cleaner — the shard jobs already
  emit associative partial tallies (like MediaReview's appended tallies / WordCount's
  word counts), so the native merger is a generic associative fold over the partials.
- **Requirement on the workload:** it must be **divide-and-merge-able** — the result
  is an associative/commutative combine of per-shard partials (counts, sums, top-k,
  keyed-state last-write-wins, etc.). Stated up front as the contract, same as the
  existing cross-node `Aggregate`. Workloads that aren't reducible this way are out
  of scope.

### B.3 Why native (not WASM) for the merge
- The shard *compute* still runs in WASM (zero-copy SHM, the existing engine).
- Only the *final fold* runs native, where the data may exceed 4 GiB. This keeps the
  fast path in the engine and puts only the unavoidable big-memory step outside it —
  matching the user's framing ("the jobs can't do it in wasm, use a standard process").

### B.4 Phasing
1. Land Part A (parallel shards) first.
2. Group submit + back-pressured scheduling of shards across slots.
3. Native merger: start with a generic associative fold over partial-tally files
   (covers WordCount, MediaReview, TF-IDF, FINRA — all already emit reducible partials).
4. Wire a `submit-group` CLI + a results contract doc.

---

## Milestones (suggested order)
- **M1** Part A.1 — single-box parallel jobs (SHM-slot + agent job maps), no RDMA.
- **M2** Part A.2 — mesh port-slotting; two RDMA cluster jobs in parallel.
- **M3** Part A.3 — admission control (slots + memory) + status for K jobs.
- **M4** Part B — job-group submit + native associative merger.

## Open questions
- `MAX_PARALLEL` and `MAX_NODES` sizing vs the OS ephemeral-port range (drives the
  mesh base/span choice).
- Per-node memory cap for admission — fixed config, or derived from live free RAM?
- Merger placement — always on the coordinator, or the least-loaded node (it needs
  to pull all partials)?
- Failure semantics for a group — fail-fast vs. retry a failed shard in a freed slot.
