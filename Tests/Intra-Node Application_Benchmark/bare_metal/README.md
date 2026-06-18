# Bare-metal reference — WordCount (single process, multi-threaded Python)

The **floor** for the WordCount suite: a plain program on one box — **no framework,
no KVS, no WASM, no shared-memory page chain, no IPC**. Just `re` + `collections.Counter`
across a `ThreadPoolExecutor`. It exists so the distributed / zero-copy systems
(WebAsShared, RMMap-ES, Faasm-like, Cloudburst) can be read against what a single
machine does with nothing in the way.

Same workload shape as the others — splitter → mapper ×N → reducer:

```
read corpus (RAM)                                   ← excluded from the timer
 → split   : N contiguous, newline-aligned ranges over the one in-RAM buffer
 → map ×N  : ThreadPoolExecutor(N); each thread counts [a-z]+ in its range
             (C-level re.findall + Counter, scanned in 8 MiB newline-aligned windows
              so a 1 GB corpus never explodes to one Python object per token)
 → reduce  : merge the N partial Counters
```

`wordcount.py` runs one `(corpus, N)` measurement and prints
`compute_ms=… total_occurrences=… unique_words=…`; `run.sh` is the sweep harness
(it also samples peak RSS out-of-band at 50 ms, the same way the other systems' `run.sh`
sample the host process tree).

## The gate (same tokenization as every system)

Maximal `[a-z]+` runs over ASCII-lowercased text — **identical to the guest and the
Cloudburst baseline**, so `total_occurrences` is fan-out-invariant and matches the
suite gate exactly: **8,940,339** @50 MB, **89,403,388** @500 MB (179,060,000 @1 GB).

## The point: Python threads can't parallelize this

`re` and `bytes.lower()` hold the **GIL**, so the map threads **serialize** — and worse,
contend. `compute_ms` is **flat-to-rising in N**: more threads never help and usually hurt.
**N=1 is the real single-thread reference.** This flatness is the finding — it is *why*
every other system in the suite fans out across **processes** (RMMap pods, Faasm Faaslets,
WebAsShared subprocess workers), not threads. To actually use the cores from one process
you'd need `multiprocessing` (pay pickling/IPC) or release the GIL in a native extension —
i.e. you'd be reinventing what the frameworks provide.

## Results (this box; quick run — reps=1, N∈{1,4,16})

```bash
./run.sh "corpus_large.txt corpus_xlarge.txt" "1 4 16" 1      # what produced results.csv
./run.sh "corpus_large.txt corpus_xlarge.txt corpus.txt" "1 2 4 8 16" 3   # full sweep (slow at 1 GB)
```

| size  | N=1 (s) | N=4 (s) | N=16 (s) | peak RSS (N=1) | occurrences |
|-------|---------|---------|----------|----------------|-------------|
| 50 MB | **3.46** | 4.94   | 5.63     | 162 MB         | 8,940,339   |
| 500 MB| **34.0** | 48.8   | 50.9     | 612 MB         | 89,403,388  |

Best latency is always **N=1**; N>1 is slower (GIL contention) — the flat curve is the result.

## Where bare-metal sits (best-over-W compute/e2e, this box)

| system (best W)        | 50 MB | 500 MB | scales w/ workers? |
|------------------------|-------|--------|--------------------|
| WebAsShared (AOT)      | **0.58 s** | **4.83 s** | yes (processes, zero-copy SHM) |
| Faasm-like (wasm+Redis)| 0.78 s | 7.68 s | yes |
| RMMap-ES (Redis pods)  | 3.01 s | 20.9 s | yes |
| **bare-metal (this)**  | **3.46 s** | **34.0 s** | **no — GIL, N=1 is best** |
| Cloudburst (Redis)     | 5.36 s | 51.9 s | weak |

Read-out: a plain single-process Python program lands **mid-pack** — it beats the
heaviest Redis-serializing path (Cloudburst) and is ~on par with RMMap-ES at 50 MB, but
loses **~6–7×** to the WASM / zero-copy systems and, unlike all of them, **cannot turn its
16 cores into speedup**. Memory is its one bright spot: the corpus is resident **once**
(162 MB @50 MB, 612 MB @500 MB at N=1), no KVS duplication — though it climbs with N as
per-thread window/token transients pile up (500 MB: 612 MB @N=1 → 1.3 GB @N=16).

## Honest scope

- **reps=1** here (a quick look); use the full-sweep command above (reps=3, N=1..16, +1 GB)
  for paper-grade medians. Latency varies a few % run-to-run.
- `compute_ms` excludes the corpus **disk read** (only "input staging" is outside the timer,
  matching every other system); it covers split + threaded map + merge.
- Peak memory is this process's RSS (no SHM term — bare metal has none); the other systems'
  memory uses their own definitions (see the WordCount README), so treat the memory column
  as directional.
- Cross-system numbers above are each system's own `results*.csv` on this box: WebAsShared =
  the AOT line; baselines = their `baseline/*/results.csv`.
