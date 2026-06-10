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
- [ ] **Phase 0** on the eval cluster (lock nodes + RDMA, `./build.sh`, stand up RMMap/Faasm/Cloudburst).
- [ ] Write `gen_dag.py` (parametrize corpus + `fanout:N`) and `run.sh` (sweep → `results.csv`) for our side.
- [ ] RMMap: point its splitter at our `TestData/corpus_*`, sweep `mapperNum` × `PROTOCOL` ∈
      {`DMERGE`, `ES`}, collect reducer profile → `baseline/rmmap/results.csv`.
- [ ] Faasm: build the func, upload corpus state, sweep N → `baseline/faasm/results.csv`.
- [ ] Cloudburst: start local cluster (Redis), sweep `WC_NUM_MAPPERS` → `baseline/cloudburst/results.csv`.
- [ ] `plot.py`: throughput / memory / serialization-cost vs N, ours vs the three baselines.

> **Note on builds.** The Faasm and Cloudburst runs need their full stacks (Faasm WASM toolchain;
> Cloudburst local cluster + Redis), which aren't installed in this dev environment — the ports are
> written, wired into each system tree, and logic-verified, but their *end-to-end* runs happen in
> Phase 0 on the eval cluster. Our side is verified here.
