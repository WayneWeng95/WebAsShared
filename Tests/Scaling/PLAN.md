# Scaling Study — strong & weak scaling of the WasMem framework

**Status: PLAN (not yet executed).** This folder measures how the framework
scales as parallelism grows from **16 → 144 threads** on the 9×16-thread cluster,
for two workloads with opposite data-movement profiles:

| workload | type | story |
|----------|------|-------|
| **WordCount** | batch map-reduce | bandwidth-bound; RDMA/SHM page-chain merge |
| **MediaReview** | streaming (keyed state) | coordination-bound; RDMA partial-tally merge |

Harness conventions, scripts, and CSV shapes mirror
[`Tests/Scheduling_Policy/`](../Scheduling_Policy/) (wc) and
[`Tests/Streaming Application_Benchmark/inter-node/`](../Streaming%20Application_Benchmark/inter-node/)
(streaming) — we reuse their generators (`gen_variants.py`, `gen_cluster_dag.py`)
and the `gen_variants → partition → submit → time` machinery, not new code paths.

---

## 1. Thread → node mapping (the scaling axis)

Each node has **16 hardware threads** (`nproc = 16`). The cluster is **9 nodes**.
The four scaling points are reached by **adding whole nodes** (16 threads each),
i.e. this is inter-node scaling at a fixed 16 threads/node:

| threads | nodes | WordCount fanout (map workers) | MediaReview workers (nodes × parts, parts=16) |
|--------:|------:|-------------------------------:|----------------------------------------------:|
| 16  | 1 | 16  | 1 × 16 = 16  |
| 48  | 3 | 48  | 3 × 16 = 48  |
| 96  | 6 | 96  | 6 × 16 = 96  |
| 144 | 9 | 144 | 9 × 16 = 144 |

- **WordCount**: threads = total map fanout; `balanced` policy + `PACK_CAP=16`
  spreads exactly 16 map workers per node. (`balanced` is the placement-insensitive
  control already validated in `Scheduling_Policy`.)
- **MediaReview**: threads = total `*_apply` workers = `--nodes N × --parts 16`.

Each thread point requires nodes `0..N-1` up. We keep all 9 daemons up and let
`partition --nodes N` place work on the first N (nodes N..8 stay idle); the
coordinator distributes per-node DAGs only to the placed nodes.

---

## 2. Strong scaling — fixed total work, more threads

Total problem size **held constant** while threads go 16 → 144. Ideal: makespan
falls linearly (2× threads → ½ time).

- **WordCount**: fixed corpus for all points = **2 GiB** (`scaling/corpus_strong_2048mb.txt`,
  cut by `prep_corpora.sh`). At 144 threads that is ~14 MB/worker — enough to amortize
  the ~3.8 s/node fixed cost (WASM JIT/spawn), so speedup stays near-linear (5.8× at 9×).
  At 16 threads the whole 2 GiB loads on one node (~5.6 GB peak RSS+SHM, under the
  3.48 GiB SHM window). *(500 MB was the original size; it plateaued at 2.7× because each
  worker got only ~3.5 MB and the fixed cost dominated — see the input-size finding.
  4 GiB overflows the 1-node window and crashes the coordinator, so 2 GiB is the ceiling.)*
- **MediaReview**: fixed total event stream, e.g. **2,000,000 events**, split
  round-robin into N disjoint slices (`run-cluster.sh` already does this). Metric is
  throughput (events/s) at max rate; makespan is the wall time over the fixed stream.

**Derived metrics** (`p0 = 16` threads is the reference):
- `speedup(p)  = T(16) / T(p)`
- `efficiency(p) = speedup(p) · 16 / p`   (1.0 = perfect linear scaling)

## 3. Weak scaling — fixed work per thread, more threads

Per-thread work **held constant**; total grows ∝ threads. Ideal: makespan stays
flat, aggregate throughput grows linearly.

- **WordCount**: fixed corpus **per node**. Base = **256 MB/node** → total
  {256, 768, 1536, 2304} MB at {1, 3, 6, 9} nodes. We pre-cut these four corpora
  (line-aligned) from `corpus_4gb.txt` once (prep step §5). Each node's
  `placement:"all"` loader reads a 1/N line-aligned slice, so per-node bytes ≈ 256 MB
  at every point.
- **MediaReview**: fixed events **per node**, e.g. **500,000 events/node** →
  total {0.5, 1.5, 3.0, 4.5} M events (`EVENTS = 500000 × N`).

**Derived metrics**:
- `weak_efficiency(p) = T(16) / T(p)`   (1.0 = flat = ideal)
- aggregate throughput should rise ~linearly with threads.

## 4. Correctness gates (every cell must pass or it is flagged, not just timed)

- **WordCount**: a single-node ground-truth run fixes the expected reduced
  word-count total; each distributed cell must reproduce it exactly.
- **MediaReview**: the merger's `mr_summary` gate (`total_events == EVENTS`,
  `login_ok == #login events`), as in the streaming suite.

Reps: 3–5 per cell, report **median** makespan plus min/max (matching the existing
sweeps).

---

## 5. Files to create in this folder

```
Tests/Scaling/
├── PLAN.md                  # this file
├── README.md               # short how-to-run + result shape (after first run)
├── prep_corpora.sh         # cut weak-scaling corpora {256,768,1536,2304}MB from corpus_4gb.txt (line-aligned)
├── wc_scaling.sh           # WordCount strong+weak: THREADS→(NODES,FANOUT), gen_variants→partition→submit→time
├── stream_scaling.sh       # MediaReview strong+weak: THREADS→(NODES,EVENTS), reuse gen_cluster_dag→submit→time
├── analysis/
│   ├── analyze_scaling.py  # logs/CSVs → speedup & efficiency columns
│   ├── plot_scaling.py     # 2×2: strong speedup, strong efficiency, weak time, weak throughput (wc + mediareview)
│   ├── results_wc_strong.csv
│   ├── results_wc_weak.csv
│   ├── results_stream_strong.csv
│   ├── results_stream_weak.csv
│   └── figs/
```

`wc_scaling.sh` reuses `../Scheduling_Policy/gen_variants.py`;
`stream_scaling.sh` reuses `../Streaming Application_Benchmark/inter-node/gen_cluster_dag.py`
+ `../Streaming Application_Benchmark/intra-node/gen_events.py`. No guest/host
rebuild needed — same binaries as those suites.

**CSV columns** (shared shape, `mode ∈ {strong,weak}`):
```
workload,mode,threads,nodes,total_input,per_thread_input,wall_ms_median,wall_ms_min,wall_ms_max,throughput,speedup,efficiency,success,reps
```

---

## 6. Run order (after this plan is approved)

```bash
cd Tests/Scaling
./prep_corpora.sh                       # one-time weak-scaling corpora (node-0)
# cluster up: all 9 daemons (coordinator + 8 workers) via ../../cluster_start.sh

THREADS="16 48 96 144" ./wc_scaling.sh strong 3
THREADS="16 48 96 144" ./wc_scaling.sh weak   3
THREADS="16 48 96 144" ./stream_scaling.sh strong 3
THREADS="16 48 96 144" ./stream_scaling.sh weak   3

python3 analysis/analyze_scaling.py     # → speedup/efficiency columns
python3 analysis/plot_scaling.py        # → analysis/figs/scaling.pdf
```

## 7. Open decisions / risks

1. **WordCount strong-scaling input size** — fixed at **2 GiB**
   (`scaling/corpus_strong_2048mb.txt`). 2 GiB is the largest fixed corpus that both
   loads on one node (16t baseline) and runs without crashing the coordinator;
   it amortizes the per-node fixed cost so strong speedup reaches 5.8× at 144 threads.
2. **Weak-scaling base unit** — 256 MB/node (wc) and 500k events/node
   (MediaReview). At 144 threads weak-WC total = 2304 MB (cut from the 4 GB corpus);
   per-worker heap stays ~16 MB (256 MB/16 workers), well clear of the ceiling.
3. **Subset-of-cluster placement** — assumes `partition --nodes N` + all 9 daemons
   up runs cleanly with N<9 (idle nodes get no DAG). Verify on the first 3-node
   cell before the full sweep; fallback is `cluster_start.sh --only 0,..,N-1`.
4. **Baselines out of scope for v1** — this study characterizes *our* framework's
   scaling. Faasm/RTSFaaS scaling overlays can be added later by reusing the
   sibling suites' baselines.
```
