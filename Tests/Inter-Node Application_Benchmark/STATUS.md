# Inter-Node Application Benchmark — Status

WasMem (our auto-placement framework) vs the Faasm baseline, on the same 4-node
cluster, same workloads, measured the same way. Last updated **2026-06-20**.

- `faasm/`   — Faasm baseline drivers (per-node agents + Redis state). `results*.csv`.
- `wasmem/`  — WasMem drivers (coordinator + RDMA state). `results*.csv`, `dags/`.
- `scripts/` — `gen_variants.py` + per-workload DAG generators (balanced policy).

## Measurement methodology (both frameworks)

Goal: **end-to-end job makespan**, excluding the offline scheduler/partitioner.

| | Faasm | WasMem |
|--|--|--|
| Timed window start | `t0` **before** the Redis upload of input | coordinator `job_start` (after offline partition + worker-DAG dispatch) |
| Timed window end | last Faaslet completes (result written) | all workers complete; `save` Output node written |
| Makespan source | wall clock around launch→poll loop | coordinator Job Summary `total wall time` |
| `total_job_ms` | Σ per-**process** Faaslet wall times | Σ per-**node** executor durations |
| Excluded | (no scheduler) | the partitioner (offline), worker-DAG dispatch |
| Per-rep hygiene | `FLUSHDB` before each rep (cold Redis) | fresh submit each rep |

⚠️ **Two asymmetries to keep in mind when comparing:**
1. **Input staging:** Faasm's makespan *includes* the Redis upload; WasMem's *excludes*
   the initial RDMA corpus staging (it happens before `job_start`). WasMem still wins
   at 4 GB despite this.
2. **`total_job_ms` granularity:** WasMem = one executor/node (≈ few × makespan);
   Faasm = one process/worker (scales with worker count). **Compare makespan, not
   total_job_ms.**

Both frameworks run **AOT** (Faasm `wc.cwasm` via `wasmtime compile`; WasMem `--aot`
precompiles `guest.wasm`→`.cwasm`). Tokenizers are **aligned** (see fixes).

## Workload status

| Workload | Faasm | WasMem | Head-to-head |
|----------|-------|--------|--------------|
| **WordCount** | ✅ done, 50 MB–4 GB @ 60 | ✅ done, 4 GB @ 60 (overlap fix) | ✅ 4 GB: WasMem 12.0 s **beats** Faasm 15.7 s |
| **FINRA** | ✅ done, 10k–10M trades | ❌ OOMs ≥ ~384 MB (stateful rules) | ⛔ blocked — needs guest streaming fix |
| ml_training | — | gen script exists | not run this session |
| ml_inference | — | gen script exists | not run this session |
| matrix | — | gen script exists | not run this session |
| terasort | — | gen script exists | not run this session |

## Results so far

**WordCount — Faasm, 60 mappers (15/node), 4 nodes** (`faasm/results.csv`):

| Size | Makespan | Occurrences | ✓ |
|------|----------|-------------|---|
| 50 MB | 988 ms | 8,940,339 | ✓ |
| 500 MB | 2,597 ms | 89,403,388 | ✓ |
| 1 GB | 4,488 ms | 178,806,775 | ✓ |
| 3 GB | 12,144 ms | 536,420,323 | ✓ |
| 4 GB | 15,727 ms | 715,227,097 | ✓ |

**WordCount — WasMem, fanout 60, balanced, 4 nodes** (`wasmem/results.csv`):

| Size | Makespan | Occurrences | ✓ |
|------|----------|-------------|---|
| 4 GB | **12,015 ms** | 715,227,097 | ✓ |

(Counts match Faasm exactly. Only 4 GB recorded post-overlap-fix; smaller sizes need
re-running — earlier fanout-40 numbers are stale/pre-fix.)

**FINRA — Faasm, fixed 8-rule fan (2/node), 4 nodes** (`faasm/results_finra.csv`):

| Trades | Makespan | Violations | ✓ |
|--------|----------|-----------|---|
| 10k | 484 ms | 6,183 | ✓ |
| 100k | 624 ms | 49,763 | ✓ |
| 1M | 2,967 ms | 456,574 | ✓ |
| 10M | 23,858 ms | 4,529,641 | ✓ |

**FINRA — WasMem:** no valid datapoint — OOMs at 10M (see Known Issues).

## Fixes made this session (with locations)

1. **Faasm measurement boundary** — `faasm/run_wordcount.py`, `run_finra.py`,
   `faasm_lib.py`, `README.md`: `t0` moved before the Redis upload (end-to-end),
   `FLUSHDB` per rep, result saved to `TestOutput/`, added `total_job_ms` column.
   Fixed missing `import time` in `run_finra.py`.
2. **Tokenizer alignment** — `Tests/Intra-Node Application_Benchmark/WordCount/baseline/faasm/demo/wc.rs`:
   Faasm now tokenizes like the WasMem guest (whitespace-split, strip non-alpha within
   token: `don't`→`dont`). Rebuilt `wc.cwasm`, staged to workers via agent `/stage`.
3. **WasMem >2 GB RDMA staging bug** — `NodeAgent/agent/src/coordinator.rs`
   (`stage_shared_inputs_rdma`): a single RDMA WRITE > 2 GiB (IB max-message limit;
   `ibv_sge.length` is u32) was silently dropped (SUCCESS completion, no fallback), so
   ≥3 GB corpora produced only node 0's slice. Fix: **chunk the WRITE into ≤1 GiB
   segments**. Rebuilt `node-agent` (node 0 only).
4. **WasMem node-0 convergence bottleneck** — `Executor/host/src/runtime/dag_runner/mod.rs`:
   RemoteRecv threads were joined at the end of their spawn wave (wave 0), making the
   gather a **barrier in front of node 0's compute**. Fix: **defer the RemoteRecv join
   to the consuming wave** (`pending_recv`), so node 0 computes while peers' partials
   stream in. 4 GB @ 60: 20.7 s → **12.0 s** (1.72×), per-node durations now balanced.
   Rebuilt `Executor/.../host` (node 0 only).
5. **WasMem WordCount driver** — `wasmem/run_wordcount.py` + `run.sh` + `README.md`.
   Note: `word_count` is a pinned **control** in `gen_variants.py` (`--fanout` inert);
   set the DAG node `map_n.fanout` directly to widen. `--weights` enables the
   `weighted` policy (e.g. `6,11,11,12` to offload node 0).
6. **WasMem FINRA driver** — `wasmem/run_finra.py` (uses `gen_variants finra` hybrid
   sharding; fanout 60 → 60 `finra_audit_rule` workers, 15/node).

## Known issues / TODO

- [ ] **WasMem FINRA OOM (blocker).** `finra_fetch_private`/`fetch_public` (the 3
  **stateful** rules, NOT sharded by the hybrid) call `api::stream_area::chain_read_all`
  → load the *entire* trades set into wasm32 linear memory → `rust_oom` at ~384 MB
  (SHM caps at 512 MiB). Faasm survives 10M because `finra.cwasm` **streams** stdin.
  Fix: make the stateful rules stream the trades (or shard them) in `Executor/guest`.
  Until then WasMem FINRA only runs on small trade sets (≤ ~1M / 39 MB — not yet run).
- [ ] **Refresh WasMem WordCount curve** at 50 MB / 500 MB / 1 GB / 3 GB with the
  overlap fix (only 4 GB is post-fix; rest are stale fanout-40 pre-fix numbers).
- [ ] **Faasm FINRA** is capped at 8-way (one Faaslet per rule, no trade sharding). To
  scale parallelism past 8 it would need to shard trades within each rule.
- [ ] **Remaining workloads** (ml_training, ml_inference, matrix, terasort): run the
  Faasm-vs-WasMem head-to-head. Gen scripts exist in `scripts/`.
- [ ] **Durable deploy.** The coordinator (RDMA-staging) and executor (overlap) fixes
  are built **only on node 0** (correct, since the changed code paths only execute on
  the coordinator/gather node). Workers run older binaries. For durability: commit the
  source changes and rebuild/redeploy all nodes via `build.sh` + sync.
- [ ] Consider a `--expect` (true-count) gate on every WasMem run — rep-consistency
  alone masked both the >2 GB drop and the FINRA stale-result-file.

## Operational notes (cluster)

- 4 nodes (`10.10.1.2`=coord/node0, `.1`/`.3`/`.4`=workers), 16 cores each, RoCE RDMA.
- **SSH to workers is blocked** — distribute files to workers via the Faasm agent
  `/stage` endpoint (base64); the raw `/upload` needs the newer agent (workers run a
  stale one). Coordinator restarts are the user's job (workers exit on coordinator
  disconnect and can't be SSH-restarted).
- **Which fixes need which redeploy:**
  - coordinator.rs (RDMA staging) → rebuild `node-agent`, restart node-0 coordinator.
  - dag_runner (overlap) → rebuild `Executor/.../host` only; agent spawns it fresh per
    job, **no coordinator restart needed**.
  - `wc.cwasm` (tokenizer) → rebuild on node 0, `/stage` to workers; no restart (each
    Faaslet launch reads the file fresh).
- Build with the repo toolchain: `RUSTUP_HOME=/opt/myapp/.rustup
  CARGO_HOME=/opt/myapp/.cargo PATH=/opt/myapp/.cargo/bin:$PATH` (system `/usr/bin/rustc`
  lacks the wasm target).
- TestData on `/` has limited headroom (~6 GB free at 4 GB corpus). Redis persistence
  is off (no disk spill).
