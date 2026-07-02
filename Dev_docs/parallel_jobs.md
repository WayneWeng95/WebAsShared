# Parallel Jobs & Job-Level Divide-and-Merge — design plan

**Status: IMPLEMENTED & VALIDATED ON THE LIVE 9-NODE CLUSTER (2026-07-02).**
The original two "Parts" were reframed into two user goals (see next section); both
are built and proven on hardware. This document keeps the original design notes for
context; the authoritative status is the checklist below.

### Progress checklist

**Goal 1 — big-file split/merge within one job — DONE ✅**
- `host split` / `host merge` native subcommands; reducers `wordcount`, `counters`,
  `concat`, `sum`, `topk:K`, plus a workload-supplied native `merge.source` hook.
- `ShardedJob` orchestration: split → N shard executors (each own SHM) → merge.
- Memory-bounded **waves** (`max_concurrent`), **size-based auto-split**
  (`max_shard_bytes`), intermediate + SHM cleanup (coordinator and workers).
- Multi-node distribution (one shard per node, waves) — *local* only for GB shards
  until RDMA staging lands (TCP `StageFiles` size cap).
- **Validated on the real 4 GiB file** (`corpus_4gb.txt`, which a single executor
  cannot load): split→waves→merge, correct result, clean up verified. This was the
  original motivation, now proven.

**Goal 2 — concurrent independent jobs (placement model) — Phase 1 DONE ✅**
- `PlacedJob`: the coordinator places a single-node job on a node, ships input,
  runs, ships output back. Each job owns its node's socket → concurrent jobs on
  different nodes → **no demux needed** (see the placement section below).
- Async submit (thread per job), **load-aware placement** (SCX score),
  **queue** when saturated, **coordinator overflow-only** (kept free for control +
  the fan-out / cross-node aggregation that lands on node 0).
- **Validated on the 9-node cluster:** concurrent placement across workers, queue
  oversubscription, coordinator-free-until-saturated.

**Regression — normal all-nodes multi-node RDMA path** still correct (validated at
50 MB and 500 MB on the cluster; proportional, exact).

**Remaining / TODO (need the cluster to validate):**
- RDMA (vs TCP) shard staging → *distributed* sharding of GB-size shards.
- Python sharded jobs.
- Goal 2 Phase 2: concurrent **multi-node** jobs sharing workers (the reader-thread
  demux in "Part A" below); strict FIFO queue ordering.

---

## Original framing (kept for context)

Two related capabilities:

- **Part A — multi-job parallelism:** run several *independent* jobs concurrently
  under one NodeAgent (each with its own SHM region), instead of one job at a time.
- **Part B — job-level divide-and-merge:** run a workload too large for a single
  WASM guest (the 4 GiB wasm32 address-space cap) by sharding it into several
  Part-A parallel jobs and merging their partial results in a **native (non-WASM)
  process**, which has no 4 GiB ceiling.

Part B is built on Part A, so Part A lands first.

---

## Reframed direction & agreed plan (2026-07-02)

After review, the priorities were reframed. The two capabilities above map to two
concrete user goals, and the **order is reversed** from the original doc:

- **Goal 1 — big-file split/merge within ONE job (do first).** The SHM 4 GiB
  wasm32 cap blocks loading large files. For workloads where the output is smaller
  than the input and the work is splittable (wordcount, tallies, sums, top-k), run
  one job as: split the big file into `N` shards → run each shard in its **own SHM
  region with its own executor** (concurrently) → **merge** the smaller partials
  into the final result. This is the doc's "Part B", but **scoped down and pulled
  first**.
- **Goal 2 — multiple independent jobs at once (later).** The doc's "Part A" +
  admission control. Deferred behind Goal 1.

**Key simplifications agreed for Goal 1:**

1. **Split and merge run as NATIVE processes** (no WASM guest, so no 4 GiB cap),
   spawned directly by the node agent and located on the **coordinator node**.
   Implemented as new `host` subcommands `host split` / `host merge`, alongside the
   existing `compile` / `dag` / `wasm-call` dispatch in `Executor/host/src/main.rs`.
   Only the shard *compute* stays in WASM (the fast zero-copy SHM path).
2. **Single node first**, multiple nodes later.
3. **Assume the whole job fits in one node's RAM; test with a small `N`** → run all
   `N` shard executors at once. The memory-bounded "waves" scheduler (process `K`
   shards at a time when the file exceeds RAM) is **deferred**.
4. **Merge logic = "built-ins + source hook" (option c):** ship built-in reducers
   (sum / count / topk / concat / wordcount) now, and add a hook for a workload to
   supply its **own native merge source** later.
5. **Shard count is an explicit `N`** for now; size-based auto-split
   (`N = filesize ÷ SHM window`) is deferred (same machinery).

**Goal 1 phasing:**
- **P1 — the two native subcommands** (standalone, testable by hand, no agent):
  `host split <input> <N> <out_prefix>` (record/line-boundary split) and
  `host merge <reducer> <out> <partial…>` (starting with the `wordcount` reducer).
- **P2 — agent orchestration (local only):** a `ShardedJob` spec
  `{ input, shards: N, per_shard_dag, merge: {reducer|source}, output }`; the
  coordinator runs `host split` (blocking) → `N` concurrent `host dag` executors,
  each with its own SHM `{prefix}_s{i}` (reusing `ExecutorHandle::spawn`, just `N`
  of them + poll-all) → `host merge` (blocking). **No async `handle_submit`, no
  demux, no worker sockets touched** — that keeps this milestone small.
- **P3 — reducer library + source-merge hook** (the rest of option c).

**Status (2026-07-02): P1, P2, P3 all implemented and tested end-to-end.**
- P1 — `Executor/host/src/shard.rs`: `host split` (line-boundary, byte-exact) and
  `host merge` with reducers `wordcount`, `counters`, `concat`, `sum`, `topk:K`.
- P2 — `NodeAgent/agent/src/sharded.rs`: `ShardedJob` detected in
  `coordinator.rs::handle_submit` (JSON has a `per_shard_dag` key), runs
  split → `N` concurrent local executors (each SHM `{prefix}_s{i}`) → merge.
  Reuses the SubmitJob/SubmitAck/JobResult wire — no protocol/client change.
- P3 — `merge: { source: <file.rs> }` compiles a self-contained native Rust merge
  (`rustc -O`, `main` gets argv `[1]`=output, `[2..]`=partials) and runs it — no
  4 GiB cap. Set exactly one of `merge.reducer` / `merge.source`.
- **Multi-node** — `distribute: true` spreads shards one-per-node across the
  cluster: coordinator runs shard 0 locally, each other shard is TCP-staged to a
  worker and run there as an independent single-node job (no RDMA mesh); the worker
  returns its partial, and the coordinator merges. Requires `shards <= live nodes`.
  Verified on a loopback 3-node cluster: distributed result byte-identical to the
  local run. A failed/oversized submit now returns a failed `JobResult` instead of
  crashing the coordinator (hardened `handle_submit` call site + wrapped
  `run_sharded_job`).
- **Waves** — memory-bounded scheduling so shards need not all run at once:
  - LOCAL: `max_concurrent: K` runs shards in a sliding window of K executors
    (default = all at once), so a file with more shards than fit in RAM is chewed
    through K at a time.
  - DISTRIBUTED: shards run in waves of one-per-node, so `distribute` now accepts
    `shards > nodes` (extra shards land in later waves; fail-fast between waves).
  Verified on a loopback 3-node cluster: all-at-once, local-window(2), and
  distributed-2-waves produce byte-identical 5-shard results.
- **Full-system check:** the normal (non-sharded) submit path still works
  end-to-end (single-node auto-placement wordcount → success), and the hardened
  `handle_submit` call site turns a bad DAG into a logged error instead of taking
  the coordinator down.
- Intermediate shard/partial files and SHM regions are reclaimed after the merge
  (`keep_intermediates: true` to retain them for debugging); the final output is
  always kept. Workers also unlink their own SHM region after every job (normal
  and sharded), so distributed shards don't leak `/dev/shm` on worker nodes.
- **Size-based auto-split:** omit `shards` and the count is derived from the input
  size — `N = ceil(filesize / max_shard_bytes)` (default `max_shard_bytes` = 3 GiB,
  under one executor's ~3.48 GiB window). Pass explicit `shards` to override.
- Goal 1 is functionally complete for one-node and multi-node. Remaining polish
  needs the real cluster/hardware: >4 GiB validation, RDMA (not TCP) shard staging,
  Python sharded jobs.

### Submitting a sharded job

A `ShardedJob` is detected by the `per_shard_dag` key and submitted like any DAG:
`node-agent submit --dag sharded.json`. Fields:

| field | required | meaning |
|-------|----------|---------|
| `input` | yes | large input file on the coordinator |
| `shards` | no | number of shards `N`; if omitted, auto-derived from file size |
| `max_shard_bytes` | no | target bytes/shard for auto-split when `shards` omitted (default 3 GiB) |
| `shm_path_prefix` | yes | shard `i` uses SHM region `{prefix}_s{i}` |
| `per_shard_dag` | yes | template single-node DAG; `{{SHARD_INPUT}}` / `{{SHARD_OUTPUT}}` are substituted per shard, `shm_path` is overridden |
| `merge.reducer` \| `merge.source` | one of | built-in (`wordcount`/`counters`/`concat`/`sum`/`topk:K`) or a self-contained native Rust merge file (`main` gets argv `[1]`=output, `[2..]`=partials) |
| `output` | yes | final result path |
| `distribute` | no | spread shards across the cluster (waves of one-per-node); default local |
| `max_concurrent` | no | local only: max shards running at once (memory bound); default all |
| `work_dir` | no | where shard/partial files live; default `/tmp/{job_id}` |
| `keep_intermediates` | no | retain shards/partials/SHM after the run; default false |

Goal 2's control-plane demux (single reader thread per worker socket → per-job
inboxes) is analysed in Part A below but **not** needed for Goal 1, since a
single-node sharded job has no worker sockets.

> **Stale paths in the sections below:** `agent/src/*` → `NodeAgent/agent/src/*`,
> and `connect/src/*` → `Executor/connect/src/*` (constants live in
> `Executor/connect/src/mesh/mod.rs`, not `lib.rs`).

---

## 0. What already works (so we don't rebuild it)

Each job's executor is a **separate `host` subprocess** that maps its **own** SHM
file (`{shm_path_prefix}_n{node_id}`) at `TARGET_OFFSET` into **its own** address
space. So two executors with *different* `shm_path_prefix` already coexist with
fully isolated SHM regions — there is no OS-level or wasm-address-space conflict
between concurrent SHM objects. **SHM multiplexing is not the blocker;** the
single-job assumptions in the NodeAgent and the RDMA mesh are.

---

## Goal 2 — concurrent jobs via placement (Phase 1, IMPLEMENTED 2026-07-02)

Rather than time-slicing many jobs over a shared worker mesh (the demux problem
below), Phase 1 takes the simpler **placement** model: the coordinator assigns
each independent job to a *single node*, ships its input there, runs it, and ships
the output back. Concurrent jobs land on **different** nodes, so each job
exclusively owns its node's socket — no two jobs share a socket, which removes the
need for the reader-thread/inbox demux entirely.

**Submission — a `PlacedJob`** (detected by a top-level `dag` key):
```json
{ "dag": { ...single-node Executor DAG (Func nodes ok)... },
  "inputs": ["/abs/path/input"],   // shipped to the target node
  "target_node": 1 }                // optional; else the coordinator picks a free node
```
Submit with `node-agent submit --dag placed.json`. The DAG's `Output` node is
collected back to the coordinator.

**How it works (`NodeAgent/agent/src/placed.rs`):**
- Each placed submit runs on its **own thread** (`SubmitJob if is_submit_placed`),
  so the accept loop keeps serving and multiple placed jobs run concurrently.
- `CoordinatorState.reserve_node()` reserves a free node (or the requested one);
  the main-loop metrics drain **skips reserved nodes** so it can't steal the job's
  `JobCompleted`. The job thread `try_clone()`s the reserved socket and reads
  completion off it without holding the state lock.
- Coordinator-local placement runs `host dag` directly; remote placement reuses
  the Goal 1 dispatch (stage input → `AssignJob` → collect output).

**Verified on a loopback 3-node cluster:** local placement, remote placement, and
**two jobs concurrently on nodes 1 & 2** (both placed before either finished, both
correct). No regression to normal/sharded paths.

**Placement + queue (done):** placement is **load-aware** — a job takes the
least-loaded free node via `CoordinatorState::placement_order(live, coordinator_id)`
(the SCX scheduler score, same signal the partitioner uses), or an explicit
`target_node`. The **coordinator (node 0) is always ranked last** — kept free for
control duties and the fan-out / cross-node aggregation + reduce that multi-node
jobs land on it, so it's used only as overflow. When every node is busy the job
**queues** — the thread waits for a node to free up (up to the job timeout) instead
of rejecting. **Validated on the live 9-node cluster:** 6 concurrent jobs spread
across the load-ranked workers; 8 jobs → all on workers with node 0 left free;
12 jobs → 3 workers doubled up via the queue and node 0 took exactly the overflow.

**Deferred to Phase 2 (needs cluster/RDMA to validate):** concurrent *multi-node*
jobs sharing workers (the reader-thread demux below); strict FIFO queue ordering
(current queue is a fair poll, not strictly ordered).

> Note (loopback testing only): staging an input writes it to the same path on the
> "worker", and the worker cleans up staged inputs after the job — on a shared
> loopback filesystem that deletes the source. Use throwaway per-job input copies
> when testing on one box; on a real cluster each node has its own filesystem.

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
