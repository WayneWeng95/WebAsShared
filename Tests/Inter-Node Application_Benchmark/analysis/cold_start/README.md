# Cold-start experiment — all 6 workloads (Cloudburst & RMMap)

A **cold-start** variant of every inter-node workload (WordCount, TeraSort, Finra, Matrix,
ML-training, ML-inference) for the two systems that have an infra cold start. Every run starts
the workers from **nothing**, times that cold start, runs ONE workload execution, then tears
the workers down — so the reported time includes a fresh cold start each run. 4 nodes, same
inputs/fan widths as the warm runs, **3 reps** each (`cold_start_runner.py`).

- **Cloudburst cold start** = scale the executor Deployment **0 → 60 pods** and wait until all
  are `Running`, then dispatch. (The container image is kept cached on every node — re-imported
  untimed before each scale-up — so this measures pod scheduling+startup, not an image pull.)
  Pods are scaled back to 0 after every run.
- **RMMap cold start** = a fresh RDMA server **publish** (`mmap` + `reg_mr` of the corpus) each
  run — RMMap has no pods, so this is its equivalent infra cold start. Workers are SSH-spawned
  per run anyway (already cold).
- `total = cold_start + makespan` (makespan = the warm workload execution).

## Results (all 6 workloads, mean of 3 reps, ms)

| workload | input | system | cold_start | makespan | **total** | total_job |
|----------|-------|--------|-----------:|---------:|----------:|----------:|
| WordCount | 4 GB | Cloudburst | 4947 ± 137 | 36071 | **41018** | 704274 |
| WordCount | 4 GB | **RMMap** | **8635 ± 59** | 16414 | **25049** | 857181 |
| TeraSort | 1.2 GB | Cloudburst | 5057 ± 151 | 25442 | **30499** | 79084 |
| TeraSort | 1.2 GB | **RMMap** | 5452 ± 188 | 16880 | **22332** | 58032 |
| Finra | 5M | Cloudburst | 5044 ± 17 | 22831 | **27875** | 172906 |
| Finra | 5M | **RMMap** | **1785 ± 18** | 19238 | **21023** | 205099 |
| Matrix | 4096² | Cloudburst | 4842 ± 96 | 8202 | **13045** | 139321 |
| Matrix | 4096² | **RMMap** | **1357 ± 10** | 6507 | **7864** | 316561 |
| ML training | 6M | Cloudburst | 4905 ± 37 | 4988 | **9893** | 129652 |
| ML training | 6M | **RMMap** | **1081 ± 16** | 3222 | **4303** | 120755 |
| ML inference | 6M | Cloudburst | 5332 ± 620 | 2152 | **7483** | 71099 |
| ML inference | 6M | **RMMap** | **875 ± 52** | 1909 | **2784** | 48359 |

(All gates match the warm runs; cold start is the only added term.)

## Takeaways
- **Cloudburst's cold start is ~5 s and roughly CONSTANT** across all six workloads (4.8–5.3 s)
  — it's Kubernetes pod orchestration (scale 0→60 + become `Running`), independent of the job
  or input size.
- **RMMap's cold start SCALES WITH INPUT SIZE** because it is `mmap` + `reg_mr` (pinning +
  registering the corpus for RDMA): **4 GB → 8.6 s, 1.2 GB → 5.5 s, ~200 MB → 0.9–1.8 s.** So:
  - On **small/medium inputs** (Finra/Matrix/ML, ≤256 MB) RMMap's cold start is **0.9–1.8 s —
    ~3–6× cheaper** than Cloudburst's fixed 5 s.
  - On **large inputs** the lines cross: at 1.2 GB they're roughly even (~5.5 vs 5.1 s), and at
    **4 GB RMMap's reg_mr cold start (8.6 s) actually EXCEEDS Cloudburst's pod spin-up (4.9 s)**
    — pinning 4 GB for RDMA costs more than launching pods.
- **The shorter the workload, the more cold start dominates the end-to-end time.** For ML
  inference cold start is **71 % of Cloudburst's total** but only **31 % of RMMap's**; with cold
  start included RMMap finishes **2.7×** faster end-to-end on ML inference (vs the workloads
  where compute dominates and cold start is a smaller fraction).
- (`total_job` — Σ worker busy — is unchanged from the warm runs; cold start adds only to the
  end-to-end latency, not the busy work.)

## Files
`cold_start_runner.py` (the orchestrator), `results_cold_start.csv`.
