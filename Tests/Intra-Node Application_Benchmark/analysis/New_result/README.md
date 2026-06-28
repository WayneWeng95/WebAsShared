# Single-node 16-parallel — Cloudburst & RMMap (intra-node sizes)

A single-node, 16-way-parallel run of the 4 workloads using the **inter-node Cloudburst +
RMMap harnesses** (`Tests/Inter-Node deployment/`), but pinned to **one node (node-0)** at the
**intra-node input sizes** (the largest in each intra sweep). Lets these two Python baselines
sit next to the intra-node `results_aot.csv` numbers.

- **Cloudburst** = 16 executor **k8s pods** pinned to node-0 (`cloudburst-executor-single`,
  nodeSelector + a separate `cb:tasks:single` queue) — same pod/Redis mechanism as the
  inter-node bars, just on one node. Driver `--fanout 16` (Matrix `--workers 16`).
- **RMMap (RDMA)** = 16 **host processes** on node-0 (`--nodes node-0 --self-host node-0
  --fanout 16`), one-sided RDMA-READ over the RoCE NIC **in loopback** (server + clients both
  on node-0 — confirmed working). Corpus/panels published once (register-once / read-many).
- 3 reps each. Same datasets both systems.

## Results (mean over 3 reps)

| workload | input | system | makespan (ms) | total_job (ms) | gate |
|----------|-------|--------|-------------:|--------------:|------|
| WordCount | 1000 MB | Cloudburst | 20783 ± 352 | 250991 | occ 165,212,254 |
| WordCount | 1000 MB | **RMMap (RDMA)** | **13462 ± 219** | 209671 | occ 165,212,254 |
| TeraSort  | 500 MB  | Cloudburst | 6538 ± 183 | 59478 | 5242880 / 3381554924 / 1 |
| TeraSort  | 500 MB  | **RMMap (RDMA)** | **4171 ± 54** | 51600 | 5242880 / 3381554924 / 1 |
| Finra     | 1M trades | Cloudburst | 6355 ± 188 | 44982 | viol 456,574 |
| Finra     | 1M trades | **RMMap (RDMA)** | **5133 ± 87** | 45806 | viol 456,574 |
| Matrix    | 2048²   | Cloudburst | 2894 ± 23 | 32256 (5.94 GFLOP/s) | 173,893,359,281 |
| Matrix    | 2048²   | **RMMap (RDMA)** | **2733 ± 18** | 34187 (6.29 GFLOP/s) | 173,893,359,281 |

(`makespan` = end-to-end wall; `total_job` = Σ worker busy time. Cloudburst `nodes_used`
corrected to 1 — the shared driver hard-codes 4.)

## Takeaways
- **RMMap-RDMA beats Cloudburst on makespan in all 4** (13.5 vs 20.8 s, 4.2 vs 6.5 s, 5.1 vs
  6.4 s, 2.7 vs 2.9 s): the corpus/panels are published once and read by one-sided RDMA, so
  there is no per-rep Redis upload + per-worker `GET`. The advantage is largest on the
  upload-heavy WordCount/TeraSort, smallest on Matrix (compute-bound) and Finra (8× input read
  dominates either way).
- **`total_job` is close** between the two (both pay the same Python compute floor —
  `wc_map` / sort / `audit` / `einsum`); RMMap is a touch lower on WordCount/TeraSort
  (no Redis serialize/contention) and a touch higher on Finra/Matrix (16 host processes on
  16 cores contend more than the pod scheduler does).

## Gates vs the intra-node `results_aot.csv` (wasm)
- **Matrix 173,893,359,281**, **Finra 456,574**, **TeraSort keysum 3,381,554,924** —
  **match the intra-node wasm gates exactly** (integer/deterministic computations).
- **WordCount occ 165,212,254** is the **Python `[a-z]+` tokenizer** value, lower than the
  intra wasm 179,060,000 — the same Python-vs-wasm split seen inter-node (each self-consistent
  across reps). Both Cloudburst and RMMap agree on 165,212,254.

## vs the OLD intra-node baselines (16 workers, end-to-end wall: old `e2e_ms` ≈ new `makespan`; ours = `compute_ms`)

| workload (size) | WasMem (ours) | **new RMMap-RDMA** | old RMMap-ES | **new Cloudburst** | old Cloudburst-ES |
|---|---:|---:|---:|---:|---:|
| TeraSort 500 MB | 2.4 s | **4.2 s** | 26.2 s | **6.5 s** | 26.4 s |
| Matrix 2048²    | 2.7 s | **2.7 s** | 3.0 s | **2.9 s** | 6.2 s |
| Finra 1M        | 1.8 s | **5.1 s** | 6.9 s | **6.4 s** | 16.5 s |
| WordCount       | 10.0 s @1001MB | **13.5 s @1000MB** | 28.0 s @500MB | **20.8 s @1000MB** | 53.8 s @500MB |

(Old baselines: `WordCount,TeraSort,Matrix,Finra/baseline/{cloudburst,rmmap}/results.csv`; ours
= each `results_aot.csv` `intra-shm` row. WordCount old baselines max at 500 MB; the new runs
are 1000 MB — 2× the data — so compare new-vs-ours at ~1 GB.)

- **New RMMap-RDMA vs old RMMap-ES:** ≈equal on compute-bound Matrix (transfer is small), but
  **~6× faster on TeraSort** (4.2 vs 26.2 s) — RDMA replaces the ES Redis serialize/deserialize
  exactly where data movement dominates.
- **New Cloudburst vs old Cloudburst-ES:** **2–4× faster** — the old intra Cloudburst ran the
  whole DAG in ONE process (e2e flat across workers: 53.1 s@w1 → 53.8 s@w16 for WordCount);
  the new run is 16 real pods, so it finally parallelizes.
- **New RMMap-RDMA ≈ WasMem on Matrix** (2.73 vs 2.75 s — RDMA erased the transfer gap, both
  bottleneck on the naive kernel). Remaining TeraSort/Finra gaps = the Python compute floor
  (WasMem runs the WASM kernel).

## Small intra-node sizes — Cloudburst warm **and** cold start (2026-06-27)

A second single-node 16-parallel Cloudburst sweep at the **smallest** intra-node input of each
workload, run **both warm and cold** so the FaaS pod cold-start penalty is visible. Same
`cloudburst-executor-single` pool (16 pods on node-0, queue `cb:tasks:single`), same drivers
(`Tests/Inter-Node deployment/k8s/cloudburst/driver_{finra,matrix,ml_inference}.py`), 3 reps each.

- **warm** = the 16 pods are already running and warmed (one warm-up rep discarded), then 3 timed reps.
- **cold** = per the cross-system definition in `Tests/Inter-Node deployment/EXPERIMENT_PLAN.md`
  §"warm/cold" (Cloudburst row): before **each** rep the pool is scaled to **0** (pods confirmed
  gone) then back to **16**, and the driver dispatches immediately — so the makespan includes pod
  scheduling + container + python-import cold-start. 3 such reps, mean ± pstdev.

Result files (one row per variant; `nodes_used` corrected to 1 — the shared driver hard-codes 4):
`finra_10k_cloudburst.csv`, `matrix_512_cloudburst.csv`, `ml_inference_cloudburst.csv` (100k + 300k).

| workload | input | variant | makespan (ms) | total_job (ms) | gate |
|----------|-------|---------|--------------:|---------------:|------|
| Finra        | 10k trades | warm | 129 ± 12  | 796  | viol 6,183 |
| Finra        | 10k trades | **cold** | **2509 ± 42** | 700 | viol 6,183 |
| Matrix       | 512²       | warm | 155 ± 6   | 590 (1.73 GFLOP/s) | 2,722,562,338 |
| Matrix       | 512²       | **cold** | **1986 ± 67** | 348 | 2,722,562,338 |
| ML-inference | 100k       | warm | 169 ± 5   | 1514 | predsum 310,304 |
| ML-inference | 100k       | **cold** | **2328 ± 172** | 1451 | predsum 310,304 |
| ML-inference | 300k       | warm | 409 ± 22  | 4514 | predsum 930,379 |
| ML-inference | 300k       | **cold** | **2784 ± 57** | 4687 | predsum 930,379 |

- **Cold adds a flat ~1.8–2.4 s pod cold-start** to every workload (scheduling + container +
  `import numpy`/redis connect before the first task drains). It dominates the small-size makespan
  (warm runs finish in 0.1–0.4 s), so cold/warm differ by **6–20×** here — the headline serverless
  cold-start cost. `total_job` (Σ pod busy time) is ~unchanged warm-vs-cold (same compute floor).
  The cold `gflops` figure is computed over the full cold-start-inclusive makespan, so it is not
  comparable to the warm GFLOP/s.
- **Gates match the intra-node values:** Finra **6,183** and Matrix **2,722,562,338** equal the
  intra wasm/cloudburst gates exactly (deterministic integer audit / int-entry product). ML-inference
  predsums (310,304 / 930,379) use the **regenerated** `ml_inference_model.csv` — self-consistent
  across reps (the same regenerated-model offset as the 6M inter-node gate 18,633,154), not the
  original-model wasm value.

Datasets generated for these (default seeds, in `TestData/`): `finra_10k.csv` (gen_trades, seed 42),
`matrix_a_512.bin`/`matrix_b_512.bin` (gen_matrix, seed 12345), `ml_inference_{100k,300k}.csv`
(first 100k/300k rows of `ml_inference_6m.csv`, shared `ml_inference_model.csv`).

## Datasets (intra-node sizes, in `TestData/`)
`corpus_1000mb.txt` (tiled seed), `terasort_500mb.txt` (seed 1234), `finra_1m.csv`,
`matrix_a_2048.bin`/`matrix_b_2048.bin` (seed 12345).
