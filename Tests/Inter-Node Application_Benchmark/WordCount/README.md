# WordCount — WebAsShared vs RMMap / Faasm / Cloudburst  *(current focus)*

Workload **#2** in [`../../../Benchmarks/workload_selection_table.tex`](../../../Benchmarks/workload_selection_table.tex).
The universal data-parallel baseline: word-frequency counting over text as a **map → reduce** DAG.

**Why first in this suite.** RMMap is our closest twin (RDMA, serialization/deserialization-free
state transfer) and ships WordCount natively, so it is the cleanest head-to-head on the exact claim
we care about — *moving the per-mapper intermediate word-count maps between stages without
serializing them*. WordCount is also already native on our side, so the only build work is the two
ports (Faasm, Cloudburst).

```
splitter (1)  ──fan-out──►  mapper × N  ──aggregate──►  reducer (1)
read corpus,                count words in              merge N partial
split into N chunks         each chunk → partial map    maps → totals
```

The interesting bytes are the **per-mapper partial count maps** flowing mapper → reducer. On our
system and RMMap-DMERGE these move serialization-free; RMMap-`ES`, Faasm-state, and Cloudburst-Redis
all serialize them — that delta is the result.

---

## WebAsShared side (already native — reuse, don't modify)

Guest functions: `Executor/guest/src/workloads/word_count.rs`
- `wc_distribute(arg)` — splits I/O slot 0 into `n_workers` **contiguous, record-aligned** stream
  slots by relinking page pointers — **zero-copy** (only the ≤1-page seam at each cut is copied).
  Peak SHM stays ≈ 1× input.
- `wc_map(slot)` — counts words in its slot, emits `"word=<w>\x1f<count>"` records to `slot+100`.
- `wc_reduce(slot)` — aggregates the merged map output into `"<word>: <count>"` lines.

Existing DAGs (run as-is):

| DAG | Notes |
|-----|-------|
| `DAGs/cluster_dag/word_count.json` / `_medium.json` / `_large.json` | single-node, hand-authored fan-out |
| `DAGs/symbolic_dag/word_count_auto_placement.json` | **preferred for sweeps** — `placement:"all"` + `fanout:N` expanded by the partitioner |
| `DAGs/rdma_workload_dag/rdma_word_count_{,medium_,large_}node{0,1}.json` | **two-node RDMA** halves (inter-node transfer) |
| `DAGs/rdma_workload_dag/rdma_py_word_count_node{0,1}.json` | PyFunc variant |

Corpora in `TestData/`: `corpus_large.txt` (50 MB), `corpus_xlarge.txt` (500 MB), `corpus.txt`
(1 GB). The sweep varies the corpus (input size) and the mapper count `fanout:N` (parallelism).

**Run (smoke test, intra-node) — verified working:**
```bash
./build.sh                                              # once
./node-agent run DAGs/workload_dag/word_count_demo.json   # single-node, 10 mappers
#   → output: TestOutput/word_count_result.txt
#   → timing: stdout "[DAG][timing] TOTAL compute: <ms>"; metrics jsonl: rss_bytes + shm_bump_offset
./node-agent run DAGs/symbolic_dag/word_count_auto_placement.json   # partitioner-placed variant
```
> Verified 2026-06-10: `word_count_demo.json` ran end-to-end (load → distribute → 10× map →
> aggregate → reduce → save), `TOTAL compute ≈ 11.1 s` on `TestData/corpus.txt`, result written to
> `TestOutput/word_count_result.txt`.
Inter-node (RDMA, 2 nodes): submit the `rdma_word_count_*_node0/node1` pair to a coordinator/worker
pair (same mechanism as the RDMA suites under `../../RDMA-InputScale/`).

This folder's `run.sh` will sweep `(corpus size × fanout)` over these DAGs and write `results.csv`
in the shared column shape (`size_mb,workers,topo,compute_ms,throughput,peak_mem_mb,reps`).

---

## Baseline 1 — RMMap (primary, native)

Base functions: **`Benchmarks/RMMap/wordcount/`** (`functions.py`, `app.py`, `util.py`,
`service.yaml`) — the exact starting point named in the task. It's a Knative MapReduce app:
`splitter → mapper×N (Parallel) → reducer`, dispatched by `CE_TYPE`, with the transport chosen by
the `PROTOCOL` env / `knative-configmap`:

| `PROTOCOL` | Transport | Role in our comparison |
|-----------|-----------|------------------------|
| `DMERGE`      | RDMA remote-memory-map, **serialization-free** (`util.push`/`pull`/`fetch`) | **RMMap's best mode — the head-to-head against our zero-copy/RDMA path** |
| `DMERGE_PUSH` | RDMA, push variant | ablation |
| `RRPC` / `RPC`| RPC, data passed in the CloudEvent | RPC ablation |
| `ES`          | Redis (`redis_put`/`redis_get`) + `pickle` | the **serialized** baseline (their S3/Redis path) |

Run recipe (lands in `baseline/rmmap/`):
1. Build the image from `Benchmarks/RMMap/wordcount/Dockerfile` → push to the cluster registry
   (`val01:5000/dmerge-wordcount`).
2. `kubectl apply -f service.yaml` (sets `mapperNum`, `protocol`, `heapSizeHex`, `LD_PRELOAD`).
3. Trigger via the ping-source; reducer logs `workflow e2e time` + per-part profile
   (`execute_time` / `es_time` / `sd_time` / `push_time` / `pull_time`) — already split exactly into
   our metric buckets (execute / external-store / serialize / deserialize).
4. Sweep `mapperNum` (= our `fanout:N`) and the input text size; repeat ≥5×.
   **Match the corpus** to ours (same text/size) for fairness — replace the hard-coded
   `datasets/...` path with our `TestData/corpus_*` so both systems count the same input.

Metric mapping: RMMap's `sd_time` **is** the serialization cost we report as **zero**; its
`es_time` is the external-store round-trip our page-chain avoids.

## Baseline 2 — Faasm  *(ported ✓ — see [`baseline/faasm/`](./baseline/faasm/))*

Faasm shipped matrix-multiply, ml-training-sgd, tflite-inference but **not** WordCount, so we wrote
one: a Faaslet **WASM C++** map-reduce (`baseline/faasm/wordcount.cpp`), installed into the system
tree at `../../../../compare_system/faasm/func/wordcount/` and wired into `func/CMakeLists.txt`.

- One module, three funcs: `faasmMain` (splitter: `corpus` state → N newline-aligned `chunk_<i>` →
  chain N mappers via `faasmChainThisInput` → await → chain reducer), `wc_mapper` (count `chunk_<i>`
  → `partial_<i>`), `wc_reducer` (merge `partial_*` → `result`). State moves via
  `faasmReadState`/`faasmWriteState`.
- Build/run recipe and the state-key layout are in [`baseline/faasm/README.md`](./baseline/faasm/README.md).
  Word-count + serialization logic verified with a standalone host compile.
- Metric: Faasm's **billable memory** + per-function exec time; the `partial_<i>` writes are the
  serialized state transfer (contrast to our zero-copy).

## Baseline 3 — Cloudburst  *(ported ✓ — see [`baseline/cloudburst/`](./baseline/cloudburst/))*

Cloudburst shipped summa, mobilenet, predserving but **not** WordCount, so we wrote one: a Python
map-reduce **DAG** (`baseline/cloudburst/wordcount.py`), installed at
`../../../../compare_system/cloudburst/cloudburst/server/benchmarks/wordcount.py` and wired into
`server.py` (`bname == 'wordcount'`).

- DAG `wc_split → wc_mapper_{0..N-1} → wc_reducer` via `register` / `register_dag` / `call_dag`
  (predserving.py idiom). `wc_split` puts N chunks into the KVS as `<uid>_chunk_<i>`; each mapper
  `get`s its chunk and returns a `{word: count}` dict; `wc_reducer` merges all N as positional args.
- Backing store = **Redis** (suite decision — see top-level README caveat); partial dicts cross the
  KVS serialized. Run recipe in [`baseline/cloudburst/README.md`](./baseline/cloudburst/README.md).
  Split/map/reduce logic verified with a mock-KVS harness.
- Metric: end-to-end + per-function time (`utils.print_latency_stats`), peak memory, Redis put/get
  bytes (the serialized transfer). Do **not** compare against Anna-locality numbers.

---

## Metrics & sweeps (this workload)

- **Sweeps:** input size {50 MB, 500 MB, 1 GB} × parallelism N {1, 2, 4, 8, 16}; topology
  {intra-node SHM, inter-node RDMA} on our side (baselines per their native topology).
- **Report:** throughput (MB/s and words/s), e2e runtime, peak/billable memory, RDMA bytes
  (ours + RMMap-DMERGE), and **serialization cost** (ours = 0; RMMap-`ES` `sd_time`, Faasm state
  ser, Cloudburst Redis ser). ≥5 reps, median + spread.
- **Headline comparison:** our zero-copy page-chain (intra) / serialization-free RDMA (inter) vs
  RMMap-DMERGE (closest), with RMMap-`ES` / Faasm / Cloudburst showing the cost of serializing the
  mapper→reducer maps.

## Step-by-step

- [x] All four systems have a comparable WordCount (Faasm + Cloudburst **ported**; RMMap + ours native).
- [x] Validate our side: `node-agent run DAGs/workload_dag/word_count_demo.json` → ran end-to-end,
      result in `TestOutput/word_count_result.txt` (2026-06-10).
- [x] Faasm port written + build/run recipe ([`baseline/faasm/`](./baseline/faasm/)); logic verified.
- [x] Cloudburst port written + run recipe ([`baseline/cloudburst/`](./baseline/cloudburst/)); logic verified.
- [x] Write `gen_dag.py` + `run.sh` for our side (`./run.sh "<corpora>" "<fanouts>" <reps>`). **(2026-06-12)**
- [x] **Our side — full intra-node sweep DONE.** Two guest builds:
      - **`results.csv` (JIT)** — guest `.wasm`, Cranelift-JIT'd per worker process. Up to **94 MB/s**.
      - **`results_aot.csv` (AOT)** — guest precompiled to `.cwasm` (`host compile`), `WC_WASM=…/guest.cwasm`.
        Up to **104 MB/s**. This is the **fair** line vs Faasm's AOT WASM, so the plot uses it as the
        primary "ours".
      Both: size {50,500,1000 MB} × N {1,2,4,8,16}, 5 reps; counts fan-out-invariant and linear
      (8.94M / 89.4M / 179M); `serialization = 0`; low-N OOMs on big corpora (`CRASH`).
      **JIT vs AOT (peak MB/s): 50 MB 37.6→86, 500 MB 80→104, 1 GB 94→104.** The per-worker JIT is a
      *fixed* cost (compiling the ~750 KB guest in each of N processes), so it dominates at small corpus
      and explains why JIT plateaus at ~37 while AOT scales — `gen_dag.py` takes `WC_WASM`, `run.sh` takes
      `WC_CSV`.
- [x] **Cloudburst baseline — RUNNING on Redis here** ([`baseline/cloudburst/`](./baseline/cloudburst/)):
      added a Redis-backed runner (`redis_kvs.py` + `redis_runner.py`) to the Cloudburst tree that
      executes the real `wordcount.py` DAG through Redis. 50 MB × N {1,2,4,8,16}, 3 requests →
      `baseline/cloudburst/results.csv`. ~9 MB/s, **~600 MB serialized through the KVS** for 150 MB
      processed (≈4× amplification); count `8940339` matches ours exactly. *Caveat: single-process,
      so mappers run sequentially — compare per-N absolute numbers + KVS byte volume, not the scaling
      slope.*
- [x] **Cloudburst 500 MB / 1 GB** done (`baseline/cloudburst/results.csv`) — needed Redis
      `proto-max-bulk-len` raised (1 GB corpus > 512 MB default value cap). Counts match ours at every
      size (89.4M / 179M); ~9 MB/s flat, ~4× serialization amplification through Redis.
- [x] **RMMap — ES protocol RUNNING here** ([`baseline/rmmap/`](./baseline/rmmap/)): bypassed the
      private custom-CPython by building the dmerge `bindings` on stock `python:3.7` (no kernel
      module); a driver runs RMMap's real ES handlers (splitter→mapper×N→reducer) through Redis.
      50 MB × N{1,2,4,8,16}; count `8940339` matches ours. Profile splits serialization (`sd_ms`
      pickle) + external-store (`es_ms` Redis), both growing with N — what our zero-copy path = 0.
- [ ] **RMMap — DMERGE** (the RDMA serialization-free headline, our closest twin): needs the MITOSIS
      **Rust kernel module** built+`insmod`'d (parked per no-kernel-module scope). Box has RoCE
      (`mlx4_0`) + module-load caps, so attemptable later.
- [~] **Faasm — real stack BLOCKED, but a Faasm-LIKE demo RUNS** here
      ([`baseline/faasm/demo/`](./baseline/faasm/demo/)). The pinned v0.2.3 images are gone from
      Docker Hub and GRANNY (0.32/0.33) deploys but exhausts disk + needs an API port (details in
      [`baseline/faasm/README.md`](./baseline/faasm/README.md)). So we **abstracted Faasm's mechanism**:
      `wc.rs` → wasm32-wasip1, each map/reduce a fresh **wasmtime** instance (Faaslet), state through
      **Redis** (partitioned). 50/500/1000 MB × N{1,2,4,8,16}; counts validated; peaks **64–67 MB/s**
      (compiled WASM) at a **constant ~19 MB Faaslet footprint**.
- [x] **`plot.py` overlay DONE** → [`figs/wordcount_overlay.png`](./figs/wordcount_overlay.png).
      3 panels from the three `results.csv`: (1) throughput vs fan-out N at 500 MB — ours 36→80 MB/s,
      Cloudburst flat ~9, RMMap-ES *declines* 8→3 as serialization grows with N; (2) peak throughput
      vs corpus size — ours 38→94 MB/s, baselines ~7–9; (3) **serialization through the external store
      (× corpus bytes): ours 0×, RMMap-ES 1.1×, Cloudburst 4×** — the headline.

> **Correction (2026-06-12): Cloudburst is Anna-native, not Redis-native.** The suite docs said
> "Cloudburst supports Redis natively" — it does not; its state plane is the Anna KVS, and the
> `redis`/`s3` benchmarks are data-locality micro-benchmarks, not the backing store. We made the
> Redis comparison real by **adding a Redis-backed runner** to the Cloudburst tree (details +
> fidelity caveats in [`baseline/cloudburst/README.md`](./baseline/cloudburst/README.md)).
>
> **Environment note.** This dev box is a **single node** (no RDMA fabric, Python 3.14). So **our
> intra-node SHM sweep** and the **Cloudburst-on-Redis** baseline run here; **inter-node RDMA (ours),
> RMMap (Knative+RDMA), and Faasm (WASM toolchain)** are deferred to Phase 0 on the eval cluster.
> Faasm + Cloudburst ports were already written/logic-verified; Cloudburst now runs end-to-end here.
