# Inter-Node Application Benchmark — 9-node cluster (Cloudburst vs RMMap-RDMA vs Faasm)

Head-to-head of the three non-WasMem baselines on the **9-node k3s cluster** under a uniform
**16-executors-per-node cap** with **node-0 as coordinator only** (Redis + fan-out/aggregation,
never a worker). Nodes 1–8 are the 8 workers → 8 × 16 = **128 executor slots**. Inputs are the
regenerated deterministic `TestData/` (corpus tiled from the recovered 12 KB seed).

- **Cloudburst** — KVS path: executor **pods** pull tasks off Redis; inter-stage state serialized through Redis.
- **RMMap-RDMA** — zero-copy path: node-0 publishes the input as one RDMA MR; SSH-fanned worker processes RDMA-READ their shard.
- **Faasm** — Faaslet path: per-node agents launch AOT-WASM (`wasmtime` v45) Faaslets; state moves through Redis (like Cloudburst, but WASM compute).

## Cluster config (this run)

- **node-0 = coordinator** (`10.10.1.2`): Redis KVS + the driver; excluded from all worker
  placement. Nodes 1–8 = workers.
- **16/node hard cap**, applied to every framework:
  - **Cloudburst** — k8s Deployment, `topologySpread` `DoNotSchedule` maxSkew-1 over the 8
    workers + control-plane `nodeAffinity` → exactly 16 pods/node, 0 on node-0.
  - **RMMap** — SSH round-robin over the 8 worker IPs only; fanout ≤ 128 ⇒ ≤ 16/node, with a
    hard `SystemExit` guard in the drivers.
  - **Faasm** — all 6 gens place map/compute Faaslets over the 8 workers only (`fl.map_nodes`,
    node-0 excluded — reduce stays on node-0); hard ≤16/node guard (`run_faaslets` + inline in
    wordcount). Runtime **pinned to wasmtime v45.0.0** (the version the `.cwasm` were built with;
    v46 rejects them — ML modules recompiled from source); Redis via the NodePort (30679).
  - **Knative** — `nodeAffinity` (no control-plane) + `topologySpread` in the service pods.
  - **StateFun** — intra-node streaming baseline, parallelism 2 (inherently ≤ 16).
- **Fanouts** (all systems, matched): wordcount 120 · finra 118 · ml_training 120 ·
  ml_inference 120 · matrix 128 (8×16 grid) · terasort 4.
- **Cloudburst executor pool**: 1 CPU + **4 GiB** RAM per pod (the pool also runs the
  convergence stages — FINRA's stateful full-trades rules, TeraSort merge — where 2 GiB
  OOM-kills; single fan-out executors elsewhere stay 2 GiB).

## Measurement (Cloudburst)

Per workload, **15 reps = 3 cold + 12 warm**:
- **warm makespan** (`makespan_mean_ms`, 12 reps) — pool pre-launched, steady-state compute.
- **cold makespan** (`cold_makespan_mean_ms`, 3 reps) — executors are **launched on-demand when
  the job reaches its wave** (pool starts at 0), so pod scheduling+startup is folded into the
  makespan (serverless cold start).
- **total_job** — Σ busy worker-seconds.
- **peak memory** per run — whole-cluster `MemAvailable` low-water across all 9 nodes:
  `mem_max_mb` (most-loaded node), `mem_total_mb` (Σ per-node), `redis_peak_mb` (KVS high-water,
  a cost only Cloudburst pays).

RMMap is measured at 3 reps. Its **cold start** = the one-time RDMA MR publish/`reg_mr` (the
register-once reusable resource, the analogue of Cloudburst's on-wave pod launch); `cold_makespan`
= warm makespan + that publish. RMMap bypasses Redis (redis_peak ≈ 0).

> Fairness note: RMMap's mapper *processes* are SSH-spawned every rep (never pre-warmed), so its
> **warm** makespan already includes process spawn — the MR publish is the only warm-vs-cold
> difference, which is exactly what `cold_start_ms` captures. Cloudburst's warm reuses live pods.

## Headline — makespan (mean ± std ms)

| Workload | Size | Cloudburst warm | RMMap | Faasm | Gate |
|---|---|---:|---:|---:|---|
| WordCount    | 4 GiB corpus    | 20,972 ± 353 | **11,007 ± 282** | 15,805 ± 56 | occurrences (see note) |
| FINRA        | 5 M trades      | 21,056 ± 320 | 18,876 ± 126     | **12,946 ± 235** | violations = 2,271,415 ✓ |
| ML training  | 6 M samples     | 3,527 ± 108  | **2,146 ± 154**  | 4,681 ± 399 | weight_checksum = 1232 ✓ |
| ML inference | 6 M samples     | **1,272 ± 46** | 1,881 ± 234    | 3,132 ± 26 | prediction_checksum = 18,614,792 ✓ |
| Matrix       | 4096² (int f64) | 7,601 ± 399  | **5,660 ± 5**    | 7,037 ± 111 | checksum = 1,391,095,867,672 ✓ |
| TeraSort     | 1.2 GiB (fan 4) | 22,315 ± 409 | 16,465 ± 211     | **15,731 ± 51** | records 12,582,912 / keysum 8,115,998,987 / sorted=1 ✓ |

RMMap's register-once / read-many RDMA transfer beats Cloudburst's serialize-through-Redis path
on every bandwidth-bound workload. **Faasm** (AOT-WASM compute + Redis state) is the fastest on the
compute/parse-heavy workloads (FINRA, TeraSort) — its native Faaslets outrun Cloudburst's Python
executors — but slowest on the ML kernels. The tiny **ML inference** job inverts the RDMA win:
RMMap's per-rep setup isn't amortized, so Cloudburst's warm pod queue wins.

**Gates.** All three systems agree exactly on FINRA / ML-training / ML-inference / Matrix / TeraSort
(✓, exact across all reps). **WordCount** differs by design: Cloudburst/RMMap count 676,710,552
(shared Python word-split) while Faasm's `wc.cwasm` counts **653,560,125** — each system is
self-consistent across reps; the wasm word-splitter just tokenizes differently (same as the
prior wasm-vs-Python bars).

**Cold start.** Cloudburst pays a ~flat **~7–9 s** on-wave pod launch (only ~3 s for TeraSort's
4-pod fan), independent of data. RMMap's cold start is the RDMA MR publish, which **scales with
input size**: 0.8 s (6 M-sample ML) → 5.1 s (1.2 GiB TeraSort) → 5.9 s (4 GiB WordCount). So on
the big inputs the two cold costs converge, and RMMap's cold makespan still beats Cloudburst's
warm on the bandwidth-bound workloads (e.g. WordCount 16.9 s cold vs Cloudburst 21.0 s warm).
Faasm has no warm/cold split here — it `flushdb`s Redis and re-launches Faaslets every rep, so
its makespan is always "cold-ish" (fresh Faaslet spawn), which is why it needs no separate column.

## total_job (Σ busy worker-seconds, mean ms)

| Workload | Cloudburst | RMMap | Faasm |
|---|---:|---:|---:|
| WordCount    | 584,026 | 1,127,823 | 1,015,063 |
| FINRA        | 163,561 | 240,320 | 246,110 |
| ML training  | 122,702 | 116,377 | 63,003 |
| ML inference | 69,456  | 51,266  | 60,328 |
| Matrix       | 128,948 | 576,192 | 643,820 |
| TeraSort     | 77,294  | 56,811  | 53,018 |

## Peak memory (Cloudburst, per run)

| Workload | mem_max (MB) | mem_total (MB) | redis_peak (MB) |
|---|---:|---:|---:|
| WordCount    | 12,774 | 55,898 | 6,143 |
| FINRA        | 7,632  | 47,960 | 1,692 |
| ML training  | 924    | 8,059  | 706 |
| ML inference | 905    | 7,196  | 709 |
| Matrix       | 1,157  | 9,592  | 1,153 |
| TeraSort     | 4,847  | 15,129 | 3,581 |

`redis_peak` is the KVS-staged copy Cloudburst pays and RMMap avoids (RDMA reads the one pinned
input). The text workloads (WordCount word-splitting) balloon far above input size (pure-Python).

## Raw CSVs

- `cloudburst/cloudburst_<wl>.csv` — schema: `…,makespan_mean_ms`(warm)`,…,reps`(=12)`,
  mem_max_mb,mem_total_mb,redis_peak_mb,cold_makespan_mean_ms,cold_makespan_std_ms`.
- `rmmap/rmmap_<wl>.csv` — `…,makespan_mean_ms`(warm)`,…,reps,cold_start_ms`(RDMA MR publish)`,
  cold_makespan_ms`(warm + publish)` ` (RDMA variant).
- `faasm/faasm_<wl>.csv` — `…,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,<gate>,expect,success,reps`.

## Reproduce

```bash
# Cloudburst — drivers self-manage the 128-pod pool (cold reps launch on-wave):
bash Tests/9_cluster_results/run_cloudburst.sh      # 6 wl × (3 cold + 12 warm) → cloudburst/

# RMMap — 8-worker SSH fan, node-0 publishes the RDMA MR:
bash Tests/9_cluster_results/run_rmmap.sh           # 6 wl × 3 reps → rmmap/

# Faasm — per-node agents run wasmtime-v45 Faaslets (agents must be up on all 9 nodes;
# provision workers via deployment/faasm/setup-node.sh + the v45 binary at ~/.wasmtime45/):
bash Tests/9_cluster_results/run_faasm.sh           # 6 wl × 3 reps → faasm/
```
