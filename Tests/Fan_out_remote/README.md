# Fan-out sweep experiment (capacity-aware word count)

Evaluates how the **fan-out count** affects 2-node cluster word-count runtime
across input sizes, using the new fan-out system:
- cluster-total fan-out semantics (`fanout: N` = N workers across the cluster,
  split by capacity ≈ N/2 per node on a balanced cluster),
- core-budget cap (here: 2×(16−2) = **28**, so 5–20 never hits the cap).

Run from the project root with the coordinator started:

```bash
python3 Tests/Fan_out_remote/run_experiment.py
```

Sweeps fanout ∈ {5,10,15,20} × corpus ∈ {52 MB, 524 MB, 1 GB}, best of 2 reps.
Writes `results.csv`.

## Results (2-node, 16 cores/node)

| corpus | fanout | total ms | node0 ms | node1 ms | total_occ |
|-------:|-------:|---------:|---------:|---------:|----------:|
| 52 MB  |  5 | 2019 | 1561 | 1968 | 8 940 339 |
| 52 MB  | 10 | **1694** | 1543 | 1455 | 8 940 339 |
| 52 MB  | 15 | 1909 | 1757 | 1626 | 8 940 339 |
| 52 MB  | 20 | 1956 | 1805 | 1732 | 8 940 339 |
| 524 MB |  5 | 7833 | 7527 | 7782 | 89 403 388 |
| 524 MB | 10 | **6960** | 6659 | 6781 | 89 403 388 |
| 524 MB | 15 | 7163 | 6814 | 7111 | 89 403 388 |
| 524 MB | 20 | 7077 | 6596 | 7025 | 89 403 388 |
| 1 GB   |  5 | 14085 | 13682 | 13919 | 179 060 000 |
| 1 GB   | 10 | 12929 | 11665 | 12833 | 179 060 000 |
| 1 GB   | 15 | 11931 | 11508 | 11880 | 179 060 000 |
| 1 GB   | 20 | **11607** | 11305 | 11346 | 179 060 000 |

## Findings

- **Correctness:** `total_occurrences` is identical across all fanouts for each
  corpus (8.94M / 89.4M / 179.06M) — fan-out changes only parallelism, not the
  result. The 524 MB value matches the single-node baseline exactly (1×).
- **Optimal fan-out grows with input size.** Small input (52 MB) peaks at
  fanout 10 then regresses — per-worker spawn overhead (fork + wasmtime JIT +
  SHM mmap) outweighs the parallelism. Medium (524 MB) plateaus after 10. Large
  (1 GB) keeps improving through 20 (14.1 s → 11.6 s, ~18% faster).
- This matches the Point (5) note: fan-out helps until ~per-node core count and
  startup/bandwidth costs dominate. At 1 GB the per-node share at fanout 20 is
  ~10 workers (< 16 cores), so there is still headroom; the cap (28) would be
  the ceiling.

NOTE: per-node times here are wall-clock from the job result; they INCLUDE
staging. The executor's per-wave compute breakdown (`[DAG][timing]`, staging
excluded) is in each node's executor stdout/log.
