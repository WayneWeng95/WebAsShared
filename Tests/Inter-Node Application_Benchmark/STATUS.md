# Inter-Node Application Benchmark — Status

WasMem (our auto-placement framework) vs the Faasm baseline, on the same 4-node
cluster, same workloads, measured the same way. Last updated **2026-06-21**.

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
| **FINRA** | ✅ done, 10k–10M trades; +5M | ✅ 5M @ 60 (197 MB, under OOM ceiling); OOMs at 10M | ✅ 5M: WasMem 9.0 s **beats** Faasm 12.2 s (1.35×) |
| ml_training | — | gen script exists | not run this session |
| ml_inference | — | gen script exists | not run this session |
| matrix | — | gen script exists | not run this session |
| **TeraSort** | ✅ 32 MB @ 4 (new WordCount-shaped baseline) | ✅ 32 MB @ 4 (centralized-gather path) | ⚖️ 32 MB: **tie** — WasMem 1.32 s ≈ Faasm 1.32 s (centralized gather; see caveat) |

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
| **5M** | **12,165 ms** | 2,271,415 | ✓ |

**FINRA — 5M head-to-head, fanout 60, 4 nodes** (added 2026-06-21):

| System | Makespan (median) | Violations | ✓ | Reps |
|--------|-------------------|-----------|---|------|
| **WasMem** | **8,991 ms** | 2,271,415 | ✓ | 3/3 |
| Faasm | 12,165 ms | 2,271,415 | ✓ | 3/3 |

WasMem's first valid FINRA datapoint — **wins 1.35×**. Violation counts match exactly
across systems (and match the WasMem `--nodes 1` ground truth = 2,271,415), so the gate
is hard-`--expect`, not just rep-consistency. **Scaling caveat:** WasMem 4-node (8,991 ms)
is barely faster than its own 1-node ground truth (9,052 ms) — the 3 **unsharded stateful
rules** each re-scan the full trade set on one worker (per-node durations all ~8.5–9 s),
so they cap the makespan regardless of node count. Same architectural limit that causes
the OOM at 10M, here surfacing as a scaling wall instead of a crash.

**TeraSort — 32 MB head-to-head (new this session, 2026-06-21)** (`{wasmem,faasm}/results_terasort.csv`):

| System | 1-node (ground truth) | 4-node | gate (records, keysum, sorted) | Reps |
|--------|----------------------|--------|--------------------------------|------|
| **WasMem** | 517 ms | **1,317 ms** | 335544, 216366291, 1 | 3/3 |
| Faasm | 1,808 ms | 1,323 ms | 335544, 216366291, 1 | 3/3 |

First TeraSort head-to-head — a **tie** at 4 nodes (1.32 s vs 1.32 s); gates match exactly
across systems and against ground truth. The two systems scale in **opposite directions**:
WasMem goes 517 ms → 1,317 ms (1-node → 4-node, *slower*), Faasm goes 1,808 ms → 1,323 ms
(*faster*). Why: WasMem's multi-node TeraSort uses a **CENTRALIZED gather** (the true
all-to-all transpose deadlocks the executor's blocking RDMA transport, §gaps) — workers
route+local-combine, send ONE transfer each, then **node 0 sorts the whole dataset
single-reducer**. So distributing *adds* a cross-node hop + a non-parallel final merge
on top of WasMem's already-fast in-SHM single-node path, while Faasm genuinely parallelizes
the sort across 4 range-owners. TeraSort is therefore the one workload where WasMem does
**not** win — exactly because its real distributed shuffle is still blocked. The headline
RDMA-zero-copy advantage needs the **true all-to-all transpose** (non-blocking/phased
transfers, §gaps); until then, expect WasMem to *lose* at larger TeraSort sizes as node 0's
single-reducer merge becomes the bottleneck. **Baseline note:** the Faasm side is new code
this session — `faasm/run_terasort.py` + `faasm/ts_faaslet.py` reuse the intra-node
`ts.cwasm` (partition/merge filter) over Redis buckets (the serialized all-to-all shuffle),
distributed via `deploy.sh distribute`. Only 32 MB run so far (the only records file staged).

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

- [x] **WasMem FINRA runs up to 5M (197 MB).** ✅ 5M validated 2026-06-21 (see results).
  The OOM ceiling is still real **at 10M / 403 MB**: `finra_fetch_private`/`fetch_public`
  (the 3 **stateful** rules, NOT sharded by the hybrid) call `api::stream_area::chain_read_all`
  → load the *entire* trades set into wasm32 linear memory → `rust_oom` (SHM caps at 512 MiB).
  Faasm survives 10M because `finra.cwasm` **streams** stdin. To match Faasm's 10M reach (and
  to get inter-node *scaling* — see the 5M scaling caveat above), still need to make the
  stateful rules stream/shard the trades in `Executor/guest`. Until then, 5M is the largest
  valid WasMem FINRA point; 10M remains Faasm-only.
- [ ] **Refresh WasMem WordCount curve** at 50 MB / 500 MB / 1 GB / 3 GB with the
  overlap fix (only 4 GB is post-fix; rest are stale fanout-40 pre-fix numbers).
- [ ] **Faasm FINRA** is capped at 8-way (one Faaslet per rule, no trade sharding). To
  scale parallelism past 8 it would need to shard trades within each rule.
- [ ] **Remaining workloads** (ml_training, ml_inference, matrix): run the
  Faasm-vs-WasMem head-to-head. Gen scripts exist in `scripts/`. (TeraSort ✅ done 32 MB.)
- [ ] **TeraSort: larger sizes + the real shuffle.** Only 32 MB staged (a tie). Generate
  bigger records files to find the crossover — but expect WasMem to *lose* with size until
  the **true all-to-all transpose** lands (non-blocking/phased RDMA, §gaps); the current
  centralized gather makes node 0's single-reducer merge the bottleneck. Drivers:
  `{wasmem,faasm}/run_terasort.py`; Faasm Faaslet wrapper `faasm/ts_faaslet.py` (reuses
  intra `ts.cwasm`); both N==P (fanout=nodes). N>P needs a `ts_demux` guest (§gaps).
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
