# Scaling — strong & weak scaling of the WasMem framework (16→144 threads)

Measures how the framework scales as parallelism grows from **16 → 144 threads**
on the 9×16-thread cluster, for **WordCount** (batch, bandwidth-bound) and
**MediaReview** (streaming, coordination-bound). Full design rationale, axes, and
metric definitions are in **[`PLAN.md`](PLAN.md)** — read that first.

Thread points map to whole nodes (16 threads/node):
`16→1`, `48→3`, `96→6`, `144→9`. WordCount uses a `balanced` map fanout = threads
(16 workers/node); MediaReview uses `--parts 16` per node (16 apply workers/node).

## Run

```bash
cd Tests/Scaling

# 0. one-time: cut the weak-scaling corpora {256,768,1536,2304} MB from corpus_4gb.txt
./prep_corpora.sh

# cluster up: coordinator + 8 workers (../../cluster_start.sh), rdma_ucm on all nodes

# 1. sweeps (3 reps, median). MODE = strong | weak
./wc_scaling.sh     strong 3
./wc_scaling.sh     weak   3
./stream_scaling.sh strong 3
./stream_scaling.sh weak   3

# 2. derive speedup/efficiency + plot
python3 analysis/analyze_scaling.py     # → analysis/results_scaling.csv (+ table)
python3 analysis/plot_scaling.py        # → analysis/figs/scaling.{pdf,png}
```

Override axes via env, e.g. `THREADS="16 96" ./wc_scaling.sh strong 5`,
`STRONG_EVENTS=4000000 ./stream_scaling.sh strong`, `BASE_MB=128 ./prep_corpora.sh`.

## Output

| file | contents |
|------|----------|
| `analysis/results_wc_{strong,weak}.csv`      | WordCount per-cell makespan, throughput, speedup, efficiency, correctness |
| `analysis/results_stream_{strong,weak}.csv`  | MediaReview same shape (throughput in events/s) |
| `analysis/results_scaling.csv`               | merged tidy table with canonically re-derived speedup/efficiency + ideal columns |
| `analysis/figs/scaling.{pdf,png}`            | 2×2: strong speedup, strong efficiency, weak makespan, weak throughput |

**Correctness is gated, not just timed:** every WordCount cell must reproduce the
single-node ground-truth word-count signature; every MediaReview cell must report
`total_events == events` — otherwise the row is `success=false`.

Generated artifacts (`cluster_dags/`, `logs_*`, staged DAGs, the weak corpora)
are gitignored; the CSVs and figures are tracked.
