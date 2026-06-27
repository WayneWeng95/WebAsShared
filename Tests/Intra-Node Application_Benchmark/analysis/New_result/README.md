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

## Datasets (intra-node sizes, in `TestData/`)
`corpus_1000mb.txt` (tiled seed), `terasort_500mb.txt` (seed 1234), `finra_1m.csv`,
`matrix_a_2048.bin`/`matrix_b_2048.bin` (seed 12345).
