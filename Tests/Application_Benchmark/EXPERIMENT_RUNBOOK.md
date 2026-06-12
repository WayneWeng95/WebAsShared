# Application Benchmark Runbook — cross-system experiments + final plots

This is a **single-shot reproduction guide** for the WebAsShared (WasMem) vs.
baselines application benchmarks. It documents (a) how each system is run, (b)
the methodology/fairness decisions that make the comparison honest, and (c) how
the final PDF figures are produced. WordCount is the **worked example**; the same
recipe generalizes to the other workloads (FINRA, matrix-multiply, ML, …).

> **Goal for a future agent:** reproduce the four `results.csv` files and the
> three figures (`figs/wordcount_*.pdf`) end-to-end on this single box, matching
> the numbers in [§7](#7-reference-numbers).

All paths below are relative to `WebAsShared/` unless absolute. Worked example
lives in `Tests/Application_Benchmark/WordCount/`.

---

## 1. What gets produced

Per workload, **four** `results.csv` (one per system) + a `plot.py` that emits
three vector PDFs:

| Artifact | What |
|---|---|
| `results_aot.csv` / `results.csv` | **WasMem** (ours) — AOT guest (fair) / JIT guest (reference) |
| `baseline/faasm/demo/results.csv` | **Faasm-like** WASM+Redis demo |
| `baseline/rmmap/results.csv` | **RMMap-ES** (Redis+pickle), parallel mappers |
| `baseline/cloudburst/results.csv` | **Cloudburst** on Redis |
| `figs/wordcount_overlay.pdf` | throughput: vs-N line (500 MB) + per-size bars |
| `figs/wordcount_latency.pdf` | latency: vs-N line + per-size bars |
| `figs/wordcount_bars.pdf` | **the paper figure**: throughput bars ‖ latency bars (broken axis) |

**Correctness gate:** every system must report identical `total_occurrences` at
each corpus size — `8940339` (50 MB) / `89403388` (500 MB) / `179060000` (1 GB).
If a port's count diverges, its tokenization is wrong — fix before trusting timings.

---

## 2. Directory layout (per workload)

```
Tests/Application_Benchmark/WordCount/
├── README.md                # workload-specific notes + checklist
├── gen_dag.py               # ours: emit a native fan-out DAG (env WC_WASM overrides guest)
├── run.sh                   # ours: sweep (corpus × N) → results.csv (env WC_CSV overrides output)
├── results.csv              # ours, JIT guest
├── results_aot.csv          # ours, AOT guest  ← the line the plot uses
├── plot.py                  # makes all three figures
├── figs/                    # output PDFs
└── baseline/
    ├── cloudburst/{run.sh, run_redis.py, wordcount.py, results.csv}
    ├── rmmap/{Dockerfile, driver.py, run.sh, app/, results.csv, results_sequential.csv}
    └── faasm/demo/{wc.rs, wc.cwasm, driver.py, run.sh, results.csv, README.md}
```

Shared CSV columns (all systems): `size_mb,workers,topo,throughput_mb_s,
words_per_s,peak_mem_mb,…,total_occurrences`. Latency column differs by system
(handled in `plot.py`'s `LAT_KEY`): ours `compute_ms`, Faasm/RMMap `e2e_ms`,
Cloudburst `e2e_ms_median`.

---

## 3. Environment — one-time setup on this box

This box already has: `node-agent` + `Executor/target/release/host` built (`./build.sh`),
Docker, a host **Redis** (`redis-cli ping` → PONG), **wasmtime** (`~/.wasmtime/bin`),
`rustc` with the **`wasm32-wasip1`** target, Python 3.14 with `redis` + `numpy` +
`matplotlib`, and a **`dmerge-redis`** container (`redis:7`).

If starting fresh, ensure:

```bash
# build our system
cd WebAsShared && ./build.sh

# host Redis for Cloudburst + Faasm-demo (no auth, :6379)
redis-server --daemonize yes --save "" --appendonly no --maxmemory 24gb
redis-cli config set proto-max-bulk-len 3gb          # 1 GB values exceed the 512MB default
redis-cli config set client-query-buffer-limit 3gb   # 1 GB single SET exceeds the 1GB default

# python deps (PEP-668 → --break-system-packages)
pip3 install --break-system-packages cloudpickle matplotlib redis

# rust wasm target + wasmtime (for the Faasm demo)
rustup target add wasm32-wasip1     # wasmtime already at ~/.wasmtime/bin

# dedicated Redis for RMMap (password 'redis', on docker net 'dmerge-net')
docker network create dmerge-net 2>/dev/null
docker run -d --name dmerge-redis --network dmerge-net redis:7 \
  redis-server --requirepass redis --save "" --maxmemory 24gb
docker exec dmerge-redis redis-cli -a redis config set proto-max-bulk-len 3gb
docker exec dmerge-redis redis-cli -a redis config set client-query-buffer-limit 3gb
```

Standard corpora (already present): `TestData/corpus_large.txt` (50 MB),
`corpus_xlarge.txt` (500 MB), `corpus.txt` (1 GB) — synthetic, ~25 unique words,
all-lowercase-alpha (so every tokenizer agrees).

---

## 4. Per-system approach + exact run commands

### 4.1 WasMem (ours) — native DAG, AOT is the fair line

Native `Executor/guest/src/workloads/word_count.rs`: `Input → wc_distribute
(zero-copy contiguous split) → wc_map × N → Aggregate → wc_reduce → Output`. Each
map worker is a **separate OS process**; data moves through the SHM page-chain
with **zero serialization**.

**AOT vs JIT (critical for fairness).** By default the benchmark DAG runs
`guest.wasm`, which each worker **JIT-compiles** (Cranelift) — a fixed ~600 ms
cost that dominates at small corpus (each fan-out map is a separate process, so
JIT is paid N×). The Faasm demo uses AOT WASM, so to compare fairly we sweep our
guest **AOT-precompiled** too. node-agent's **`--aot`** flag (added 2026-06-12)
does this automatically — `host compile`s `guest.wasm → guest.cwasm` (matching
the host's wasmtime engine; cached when fresh) and rewrites the DAG's wasm_path.

```bash
cd WebAsShared && ./build.sh        # also produces guest.cwasm

cd Tests/Application_Benchmark/WordCount
# JIT sweep (reference) → results.csv
./run.sh "corpus_large.txt corpus_xlarge.txt corpus.txt" "1 2 4 8 16" 5
# AOT sweep (the line the plot uses) → results_aot.csv
WC_WASM="Executor/target/wasm32-unknown-unknown/release/guest.cwasm" \
WC_CSV="$PWD/results_aot.csv" \
  ./run.sh "corpus_large.txt corpus_xlarge.txt corpus.txt" "1 2 4 8 16" 5
```

- The benchmark harness selects AOT via `WC_WASM=<.cwasm>` (which `gen_dag.py`
  stamps into the DAG's `wasm_path`); `run.sh` honors `WC_CSV` for the output
  path. (For an ad-hoc single run, `node-agent run <dag> --aot` is the equivalent
  — it `host compile`s + rewrites the path for you.) 5 reps, median reported;
  `peak_mem_mb` sampled out-of-band at 50 ms.
- **Important:** the `.cwasm` MUST come from `host compile` (the executor's
  embedded wasmtime), **not** the standalone `wasmtime` CLI — the host loads it
  via `Module::deserialize_file`, which rejects a wasmtime-version/`Config`
  mismatch. `build.sh` keeps `guest.cwasm` in lockstep with the `host` binary.
- **Low-N OOMs on big corpora** are expected (a single mapper's `read_all`
  exceeds the guest heap) → recorded as `CRASH` rows; the plot skips them and
  takes the best (max-throughput / min-latency) N per size.

### 4.2 Faasm — real stack is dead; run the **Faasm-like demo**

Real Faasm can't run here: the pinned v0.2.x images were **deleted from Docker
Hub**, and the GRANNY (0.32/0.33) stack deploys via `faasmctl` but **exhausts the
disk** and needs a host-interface port. So we abstract Faasm's mechanism:

- `wc.rs` → **`wasm32-wasip1`**, AOT-compiled to `wc.cwasm`; each map/reduce is a
  **fresh `wasmtime` instance** (= a Faaslet, SFI-isolated).
- The host driver does the KV I/O: writes `chunk_<i>` / reads `partial_<i>` in
  **Redis** (serialized state — the cost our SHM avoids); **partitioned** reads
  (each mapper reads only its chunk, like the real `wordcount.cpp`).
- Mappers run **concurrently**; `wc.rs` **streams** stdin (a single `read_to_end`
  into wasm32 memory caps ~128 MB and silently truncates).

```bash
cd Tests/Application_Benchmark/WordCount/baseline/faasm/demo
./run.sh "50 500 1000" "1 2 4 8 16"     # builds wc.cwasm, sweeps → results.csv
```

Footprint is a constant **~19 MB** per Faaslet (Faasm's headline); measured via
`/usr/bin/time` per instance. Honest scope: no Faasm scheduler / Proto-Faaslet
snapshots. Full notes in `baseline/faasm/demo/README.md`.

### 4.3 RMMap — ES protocol only (no kernel module), **parallel mappers**

RMMap ships WordCount natively but needs Knative + the MITOSIS **Rust kernel
module** for its DMERGE (RDMA, serialization-free) headline. The box *has* RoCE
(`mlx4_0`) and module-load capability, but **DMERGE is parked** (don't build/load
the kernel module unless explicitly authorized). We run the **ES** protocol
(Redis + pickle), which needs no module (`util.py`: `SD = sopen() if PROTOCOL in
('DMERGE',…) else 0`).

The blocker for *any* RMMap run is the private custom-CPython image — **bypassed**
by building the dmerge `bindings` Cython module on **stock `python:3.7`** (the
`native/wrapper.h` is self-contained C; the syscalls aren't called for ES).
`driver.py` calls the real `splitter_es`/`mapper_es`/`reducer_es` through real
Redis, mappers **parallel** (each its own process — matching Knative pods).

```bash
cd Tests/Application_Benchmark/WordCount/baseline/rmmap
./run.sh corpus_large.txt   "1 2 4 8 16"   # builds the image + dmerge-redis, → results.csv (50 MB)
# 500 MB / 1 GB: same image, MAPPER_PARALLEL=1, append rows (corpus_xlarge.txt / corpus.txt)
```

The `run.sh` defaults to the parallel sweep; `results_sequential.csv` keeps the
single-process run for reference. Two fixes baked into `app/functions.py`:
corpus path from `WC_CORPUS`, Redis host from env, and a **per-ID output key**
(`/tmp/wc_<i>.txt`) so the N partials don't clobber in shared Redis (upstream
keyed them all the same — a latent bug; counts now correct).

> **Why parallel + why it matters:** RMMap-ES's splitter writes the whole corpus
> to one Redis key and **every mapper `redis_get`s the entire corpus** (reads =
> N× corpus, the broadcast). Run sequentially that just adds to the Python-regex
> floor; run in parallel it makes **Redis the bottleneck** — throughput peaks at
> N=8 then drops at N=16. That is the inherent ES baseline DMERGE is built to beat.

### 4.4 Cloudburst — Anna-native; run a **Redis-backed** runner

**Correction to the suite docs:** Cloudburst's state plane is the **Anna KVS**,
not Redis (the `redis`/`s3` benchmarks are data-locality micro-benchmarks). The
full Anna control plane is a 2019/Python-3.6 stack that won't stand up here. So we
**added a Redis-backed runner to the Cloudburst tree**:

- `cloudburst/shared/redis_kvs.py` — Anna-compatible `get/put` over Redis, with
  byte accounting.
- `cloudburst/server/benchmarks/redis_runner.py` — runs the **real `wordcount.py`
  DAG** (split→map×N→reduce) in-process, routing every chunk/partial **through
  Redis** (cloudpickle).

```bash
cd Tests/Application_Benchmark/WordCount/baseline/cloudburst
./run.sh "$(cd ../../../../.. && pwd)/TestData/corpus_large.txt" "1 2 4 8 16" 3
# 500 MB / 1 GB: same, with corpus_xlarge.txt / corpus.txt (append). 1 GB needs the
# raised Redis proto-max-bulk-len + client-query-buffer-limit (§3).
```

One **fresh process per N** (clean per-N peak RSS). Honest scope: single-process
(mappers sequential — its throughput is ~flat in N), Redis not Anna (don't
compare to Cloudburst's Anna-locality numbers). It moves **~4× the corpus**
through Redis (the serialization cost). Details in `baseline/cloudburst/README.md`.

---

## 5. Methodology & fairness decisions (read before changing numbers)

1. **Same corpus, same N grid** for everyone: 50/500/1000 MB × N∈{1,2,4,8,16}.
2. **Correctness first:** counts must match across systems (see §1). Tokenize on
   non-alpha + lowercase everywhere.
3. **AOT for ours** (not JIT) when comparing to AOT baselines — else our small-
   corpus number is unfairly penalized by per-worker JIT (37→86 MB/s at 50 MB).
4. **Parallel mappers** where the real system deploys per-function workers
   (RMMap pods, Faasm Faaslets). Cloudburst's local runner stays single-process —
   noted as a caveat; its curve is ~flat in N regardless.
5. **Best config per size** in the per-size bars: max throughput / min latency
   over N (skips `CRASH`/OOM cells).
6. **Redis limits** must be raised for 1 GB (`proto-max-bulk-len`,
   `client-query-buffer-limit` → 3gb) or large single values reset the connection.
7. **Don't load the MITOSIS kernel module** (RMMap DMERGE) without explicit
   authorization — it manipulates page tables + RDMA in kernel space.

---

## 6. Plotting — `plot.py` (one shot → all three PDFs)

```bash
cd Tests/Application_Benchmark/WordCount
python3 plot.py        # → figs/wordcount_overlay.pdf, _latency.pdf, _bars.pdf
```

Styling is centralized at the top of `plot.py` and **must match the StateSync
figures** (`Tests/StateSync/plot_compare.py`) for a coherent paper:

- **Colors/markers (from StateSync's own per-system assignment):**
  WasMem `#2057c7` `o`, Faasm `#2a9d8f` `D` (shm-copy), RMMap `#1d7a3e` `*`
  (shm-zerocopy), Cloudburst `#edae49` `v` (redis-local).
- **Fonts:** `TICK_SIZE=16`, `LABEL_SIZE=16`, `LEGEND_SIZE=17`, `YLABEL_SIZE=16`.
- **Legend:** frameless, at the top, **order Cloudburst → RMMap → Faasm → WasMem**
  (handles reversed); 1×4 in the bars figure, 2-col elsewhere.
- **`wordcount_bars.pdf`** (the paper figure), fixed **9×4.5** (no tight crop):
  - left = throughput per size (linear bars); right = latency per size on a
    **2-stage broken y-axis** — body `0–25 s`, band `38–56 s` (ticks 40/50),
    and the **104 s outlier capped with its value written *inside* the bar**;
  - **no panel titles** (panels identified by the y-axes: `Throughput (MB/s)` /
    `Latency (s)`); bar value labels bold, size 9.

`plot.py` reads `results_aot.csv` as "ours" (the AOT/fair line). To switch ours
back to JIT, point `OURS` at `results.csv`.

---

## 7. Reference numbers

Peak throughput (MB/s) / best latency (s), all counts validated:

| size | WasMem | Faasm | RMMap | Cloudburst |
|------|--------|-------|-------|------------|
| 50 MB  | **86** / 0.6 | 64 / 0.8 | 17 / 3.0 | 9 / 5.4 |
| 500 MB | **104** / 4.8 | 65 / 7.7 | 24 / 21 | 10 / 52 |
| 1 GB   | **104** / 9.6 | 67 / 15  | 24 / 41 | 10 / 104 |

WasMem leads on both axes at every size; the gap widens with corpus size (zero-
copy SHM vs. the baselines' serialized KV transfer).

---

## 8. Single-shot reproduction (copy-paste)

```bash
cd /opt/myapp/WebAsShared
./build.sh                                   # if not already built

# --- ours: AOT compile + both sweeps ---
./Executor/target/release/host compile \
  Executor/target/wasm32-unknown-unknown/release/guest.wasm \
  Executor/target/wasm32-unknown-unknown/release/guest.cwasm
cd Tests/Application_Benchmark/WordCount
./run.sh "corpus_large.txt corpus_xlarge.txt corpus.txt" "1 2 4 8 16" 5
WC_WASM="Executor/target/wasm32-unknown-unknown/release/guest.cwasm" \
WC_CSV="$PWD/results_aot.csv" \
  ./run.sh "corpus_large.txt corpus_xlarge.txt corpus.txt" "1 2 4 8 16" 5

# --- faasm-like demo ---
( cd baseline/faasm/demo && ./run.sh "50 500 1000" "1 2 4 8 16" )

# --- rmmap ES (parallel) : 50/500/1000 MB ---
( cd baseline/rmmap
  for c in corpus_large.txt corpus_xlarge.txt corpus.txt; do ./run.sh "$c" "1 2 4 8 16"; done )  # appends per corpus

# --- cloudburst on redis : 50/500/1000 MB ---
WAS=/opt/myapp/WebAsShared
( cd baseline/cloudburst
  for c in corpus_large corpus_xlarge corpus; do ./run.sh "$WAS/TestData/$c.txt" "1 2 4 8 16" 3; done )  # appends per corpus

# --- figures ---
python3 plot.py            # → figs/wordcount_{overlay,latency,bars}.pdf
```

> Notes for the new agent: each baseline `run.sh` **truncates** its `results.csv`
> on first call — to accumulate sizes, either pass all corpora in one invocation
> (ours) or append across calls (baselines); the per-baseline `run.sh` headers
> document their exact behavior. Verify the §1 correctness gate before plotting.
> Background the big sweeps (1 GB single-process Python is slow) and don't mutate
> a `results.csv` while a sweep is appending to it.

---

## 9. Applying this to a NEW workload

1. Make a sibling folder `Tests/Application_Benchmark/<Workload>/` with the same
   layout (§2).
2. **Ours:** author the DAG generator (or reuse a native workload) → `run.sh`
   emitting the shared columns; AOT-compile the guest.
3. **Baselines:** port the workload into each baseline tree the same way
   (Cloudburst real funcs via the Redis runner; RMMap real ES handlers via the
   py3.7 image; Faasm-like via a `wasm32-wasip1` module + wasmtime/Redis driver),
   keeping the **correctness gate** and the **fairness rules** (§5).
4. Copy `plot.py`, adjust the workload's x-axis/units and the `LAT_KEY`/columns;
   keep the StateSync palette + the bars-figure styling (§6).
5. Run §8's sequence; confirm counts; ship the three PDFs.
