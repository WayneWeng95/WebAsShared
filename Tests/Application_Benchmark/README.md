# Application Benchmark — WebAsShared vs RMMap / Faasm / Cloudburst

Cross-system **application-level** evaluation of **WebAsShared** (our WASM DAG dataflow engine with
a zero-copy shared-memory page-chain substrate and serialization-free RDMA state transfer) against
the three systems it is closest to: **RMMap**, **Faasm**, and **Cloudburst**.

This suite covers workloads **2–6** of
[`../../Benchmarks/workload_selection_table.tex`](../../Benchmarks/workload_selection_table.tex) —
the five rows whose comparison set is *RMMap / Faasm / Cloudburst*:

| # | Folder | Workload | Family | Primary baseline | Secondary baselines |
|---|--------|----------|--------|------------------|---------------------|
| 2 | [`WordCount/`](./WordCount/)     | WordCount            | data-parallel text       | RMMap (native, closest twin) | Faasm, Cloudburst |
| 3 | [`Finra/`](./Finra/)             | FINRA                | multi-stage stateful DAG | RMMap (native)               | Faasm, Cloudburst |
| 4 | [`Matrix/`](./Matrix/)           | Matrix multiplication| distributed compute      | Faasm (divide-and-conquer)   | RMMap, Cloudburst (SUMMA) |
| 5 | [`ML_training/`](./ML_training/) | ML training (SGD)    | distributed training     | RMMap `ml-pipeline`          | Faasm (HOGWILD!), Cloudburst |
| 6 | [`ML_inference/`](./ML_inference/)| ML inference (MNIST)| ML inference             | RMMap (native MNIST)         | Cloudburst, Faasm |

> Workload **1** (Image-resize/fanout vs Roadrunner) and **7** (MediaReview vs RTSFaaS) are
> single-baseline and live elsewhere (`../ImageResize_Fanout/`, plus the MediaReview plan in
> `../../Benchmarks/WORKLOAD_SELECTION.md`). They are out of scope for this folder.

**Strategy: one workload at a time** (same as `../../Benchmarks/TEST_PLAN.md`). We fully stand up,
run, and validate each workload — on our system *and* all three baselines — before moving on.
**Current focus: WordCount (#2).** The other four are a documented backlog (stub READMEs) detailed
when we reach them.

---

## What "adopting the main framework" means

For every workload, the WebAsShared side is expressed as a **DAG of guest functions** run by the
`node-agent` binary. There is no per-workload server to deploy — you build once and run a DAG:

```
./build.sh                       # at repo root → node-agent, host, guest WASM (do once)
./node-agent run DAGs/<...>.json # execute a workload DAG on this node
```

- **Guest functions** (the compute) live in `Executor/guest/src/workloads/*.rs` (native WASM) or
  `Executor/py_guest/python/*.py` (PyFunc). Several workloads already ship natively — reuse them;
  **do not** change framework code to fit a benchmark. If a workload genuinely needs a new guest
  function, add it *additively* (new `#[no_mangle]` fn, no edits to existing stages), per the
  backward-compat rule.
- **DAGs** (`DAGs/`) describe the topology: `Input → Func/StreamPipeline → Shuffle → … → Output`.
  Single-node DAGs run with `node-agent run`; two-node RDMA DAGs (`DAGs/rdma_workload_dag/`) split
  into `node0`/`node1` halves and are submitted to a coordinator/worker pair.
- **Placement.** `DAGs/symbolic_dag/*_auto_placement.json` lets the partitioner expand `placement:
  "all"` and `fanout:N` automatically — preferred for scaling sweeps so we don't hand-author N
  mapper nodes.
- **Metrics** come from `NodeAgent/agent/src/metrics.rs` (default log `/tmp/node_agent_metrics.jsonl`)
  plus the `[DAG][timing]` lines on stdout. Our headline numbers — zero serialization cost, peak SHM
  ≈ 1× input — are read from there.

## How the three baselines run (per workload, summarized)

| System | Substrate | How a workload runs | Native workloads present |
|--------|-----------|---------------------|--------------------------|
| **RMMap** (dmerge) | RDMA remote-memory-map; ser/deser-free DAG state transfer (same thesis as us) | Knative `Service` + `Sequence`/`Parallel` flows; `PROTOCOL` env selects transport: **`DMERGE`**(their best, RDMA) / `DMERGE_PUSH` / `RRPC` / `RPC` / **`ES`**(Redis, serialized). `MAPPER_NUM` sets fan-out. See `Benchmarks/RMMap/<wl>/service.yaml`. | wordcount(+java), finra, ml-pipeline, digital-minist (MNIST) |
| **Faasm** | WASM/Faaslet isolation + shared-memory state; "billable memory" metric | Faaslet WASM functions built with the Faasm toolchain, chained via host calls, state in Faasm KV. Build against `../faasm` (v0.2.4). | matrix-multiply, ml-training-sgd (HOGWILD!), tflite-inference |
| **Cloudburst** | Stateful serverless over a KVS | Python functions registered with a Cloudburst client, composed into a DAG; backing store = **Redis** (not Anna — see caveat below). | summa (matmul), mobilenet, predserving |

> **Porting gaps to close (this suite's real work).** RMMap already ships all five workloads
> natively. Faasm has matrix + ML-training + (tflite) inference but no WordCount/FINRA; Cloudburst
> has matmul (summa) + inference (mobilenet) but no WordCount/FINRA/training. Each workload's README
> lists which baseline ports must be written.
> **WordCount (#2): done** — Faasm port at `WordCount/baseline/faasm/` (installed into
> `compare_system/faasm/func/wordcount/`) and Cloudburst port at `WordCount/baseline/cloudburst/`
> (installed into `compare_system/cloudburst/.../benchmarks/wordcount.py`).

> **Cloudburst backing store = Redis, not Anna.** Anna is available but little-used; for ease of use
> every Cloudburst baseline runs on Redis (natively supported). Caveat: Anna's co-located cache is
> part of Cloudburst's locality story, so locality-sensitive numbers will differ from published
> ones. Fine for our throughput/memory comparisons — state it, and don't compare against
> Cloudburst's Anna locality results.

---

## Methodology (applies to every workload)

- **Fairness.** Same physical cluster, hardware, and input data for our system and every baseline.
  Record node count, CPU, RAM, NIC/RDMA specs once (Phase 0).
- **Common metric set:**
  - **Throughput** (records or items/s, or MB/s) and **end-to-end runtime**.
  - **Latency** — median + tail (p95/p99) for request-style stages.
  - **Memory / billable memory** — ours = `rss_bytes` (summed private RSS of the executor process
    tree) `+ shm_bump_offset` (SHM counted once); RMMap/Cloudburst/Faasm use their own peak-memory
    metric (Faasm reports "billable memory" directly).
  - **RDMA bytes transferred** (cross-node state movement) where applicable.
  - **Serialization cost** — report explicitly; **ours is zero** (the headline). RMMap `ES`/`RPC`,
    Faasm state get/set, and Cloudburst Redis put/get all serialize — that's the contrast.
  - **Scaling** — sweep parallelism (mapper/fanout count) and input size.
- **Staging excluded.** Input staging is timed separately and excluded from compute time.
- **Repeat & report.** ≥5 runs per config; report median + spread. Warm and cold where relevant.
- **Comparison scope.** Per-row: each workload is compared only against the systems in its table
  row. For this suite that is RMMap / Faasm / Cloudburst, with the per-workload primary baseline
  bolded in the table above.

## Per-folder layout (convention, mirrors `../ImageResize_Fanout/`)

```
<Workload>/
├── README.md      # workload plan: framework mapping + per-baseline run recipe + metrics
├── gen_dag.py     # build the WebAsShared DAG(s) for the sweep (parallelism × input size)
├── run.sh         # drive OUR side, write results.csv in a shared column shape
├── plot.py        # ours vs baselines
├── results.csv    # our results (generated)
├── baseline/      # baseline run scripts / collected CSVs (RMMap, Faasm, Cloudburst)
└── figs/          # generated plots
```

The baseline systems are **not** driven by our `run.sh` (each needs its own stack); `baseline/`
holds the scripts/notes to reproduce them and the CSVs they emit, normalized to the same columns so
curves overlay.

---

## Phase 0 — Shared infrastructure (do once)

- [ ] Lock the cluster: N nodes, RDMA fabric up; record specs (CPU, RAM, NIC/RDMA).
- [ ] Build our system: `./build.sh` → `node-agent`, `host`, guest WASM.
- [ ] Confirm the metric harness emits throughput / `rss_bytes` / `shm_bump_offset` per run.
- [ ] Stand up baselines **as each workload needs them** — for WordCount: RMMap (Knative+RDMA),
      Faasm (toolchain + runtime), Cloudburst (client + Redis). Don't deploy all three up front for
      later workloads.

## Status

| Workload | Our side | RMMap | Faasm | Cloudburst | State |
|----------|----------|-------|-------|------------|-------|
| WordCount    | native (`word_count.rs`, run-verified) | native | **ported** ✓ | **ported** ✓ | **all 4 have it; eval pending** |
| FINRA        | native (`finra.rs`)      | native | **ported** ✓ | **ported** ✓ | **done; all 4 reproduce gate** |
| Matrix       | native (`matrix.rs`, SUMMA) | **ES port** ✓ | **ported** ✓ (-like) | **ported** ✓ (summa) | **done; all 4 reproduce gate** |
| ML training  | native (`ml_training.rs`, SGD) | **ES port** ✓ | **ported** ✓ (-like) | **ported** ✓ | **done; all 4 reproduce gate** |
| ML inference | port needed (MNIST guest)| native | port (tflite) | port (mobilenet→MNIST) | backlog |
