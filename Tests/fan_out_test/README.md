# Fan-out sweep — map-worker parallelism

Measures how single-node word-count wall time responds to the **map fan-out**
(number of parallel `wc_map` worker *processes*), the same `fanout` knob as
`DAGs/symbolic_dag/word_count_auto_placement.json`'s `map_n` node.

`gen_wc.py` emits a flat native DAG (`load → distribute → map_n×N → aggregate →
reduce → save`) so it runs via `node-agent run` with no coordinator, isolating
the per-worker cost. `run.sh` sweeps N, runs each a few times, and records the
median wall time plus `total_occurrences` (a correctness check — it must be
constant across N).

```bash
./run.sh                                  # corpus_large (50 MiB), N=1..64, 3 reps
./run.sh corpus_xlarge.txt "2 4 8 16 32"  # 500 MiB, custom N
```

Box: 8 physical cores / 16 hyperthreads.

## Results

**50 MiB** (`results_corpus_large.csv`)

| fanout | wall ms (median) | speedup vs N=1 |
|---:|---:|---:|
| 1  | 2920 | 1.00× |
| 2  | 2150 | 1.36× |
| 4  | 1400 | 2.09× |
| 8  | 1410 | 2.07× |
| 16 | 1620 | 1.80× |
| 32 | 2180 | 1.34× |
| 64 | 3230 | 0.90× |

**500 MiB** (`results_corpus_xlarge.csv`)

| fanout | wall ms (median) | note |
|---:|---:|---|
| 1  | **CRASH** | `wc_map` OOMs the ~0.5 GiB guest heap (read_all of the whole shard) |
| 2  | 14390 | |
| 4  | 8645 | |
| 8  | 6050 | knee |
| 16 | 6110 | |
| 32 | 6785 | |
| 64 | 7425 | |

`total_occurrences` is constant across every (non-crashing) fan-out — the
zero-copy `wc_distribute` partitions the input, it never duplicates it.

## Why the curve looks like this

The fan-out has a **lower bound and an upper bound**, and is sub-linear in between:

1. **Lower bound — too little fan-out OOMs.** `wc_map` calls
   `read_all_stream_records`, which materializes its whole shard into the guest
   heap (~0.5 GiB; worker.rs splits the 4 GiB wasm32 space into 3.5 GiB SHM-min /
   0.5 GiB heap). At fanout=1 on 500 MiB the single shard is the entire corpus →
   heap OOM → `unreachable` trap. Large corpora therefore *require* enough
   fan-out to keep each shard under ~0.5 GiB.

2. **Sub-linear speedup (only ~2–2.4× even at the knee).** Amdahl: `load`
   (memcpy the whole corpus into SHM), `distribute`, `aggregate`, and `reduce`
   are serial and don't shrink with N. Decomposing the 50 MiB numbers
   (N=1 ≈ 2920 ms, N=8 ≈ 1410 ms) implies ~1.2 s of serial floor and ~1.7 s of
   parallelizable map work — so the plateau sits at the serial floor. The map
   kernel is also memory-bandwidth-bound (mostly byte-scanning + a 24-entry
   linear count), so bandwidth saturates after a few workers.

3. **Upper bound — too much fan-out regresses.** Each map worker is a separate
   OS process: fork/exec + wasmtime JIT of `guest.wasm` + `mmap` the SHM region
   + teardown. The wave spawns **all N at once with no cap**. Past ~8–16 workers
   there are no free cores, so extra workers add fixed startup cost, context
   switching, and memory-controller contention with zero throughput gain — wall
   time climbs (N=64 on 50 MiB is *slower* than N=1).

**Sweet spot ≈ physical core count (~8 here)**, bounded below by the per-shard
heap limit. The auto-placement DAG's old `fanout: 30` is well past the knee.

## Fixes that would make fan-out scale (not yet implemented)

- Stream the map kernel (`chain_for_each` instead of `read_all`) so a shard of
  any size fits a bounded heap — removes the lower-bound crash.
- Reuse one wasmtime engine/module (or run maps as threads in one process)
  to kill the N× JIT + mmap startup cost.
- Cap concurrent workers at ~core count even when fan-out is higher.
