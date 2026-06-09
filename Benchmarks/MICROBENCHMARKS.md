# Micro-benchmarks (documented, not copied)

Per request, the **application workloads** were copied into this `benchmarks/` folder, while the
**micro-benchmarks** are only documented here — what each one is, where it lives in the original
tree, and what it actually measures.

> Rule of thumb: a *workload* runs a realistic application (ML inference, a transactional app, a
> data pipeline); a *micro-benchmark* isolates one system primitive (serialization cost, raw
> transfer throughput, scheduling/scaling latency) to attribute overhead.

---

## RMMap (dmerge) — `RMMap/exp/micro` and `RMMap/exp/thpt`

### `micro/` — serialization/deserialization (ser/deser) micro-benchmark
- **Driver:** `sd_bench.py`, `sd_bench_vary_sz.py`, `functions.py`, `app.py`
- **What it tests:** the cost RMMap is designed to eliminate — **time to serialize and deserialize
  state** that crosses a function boundary, for many object representations:
  - long string / list-of-strings (`data/article.txt`)
  - NumPy `ndarray` (`data/Digits_Train.txt`)
  - pandas `DataFrame` (`data/yfinance.csv`)
  - Apache **Arrow** arrays/tables (df↔table, np↔arrow, list↔arrow conversions)
  - LightGBM booster tree (`data/mnist_model.txt`)
  - PyTorch NN state dict (`data/mnist_net.pth`)
  - deeply-nested Python dict, PIL image
  - `sd_bench_vary_sz.py` sweeps **object size** to show how ser/deser cost scales.
- **Metric:** per-object and per-batch serialize/deserialize latency (ms) and encoded byte size.
- **Point being made:** RDMA remote-memory-map transfers state with *zero* ser/deser, so this is the
  baseline overhead it removes.

### `thpt/` — throughput / pod-scaling micro-benchmark
- **Driver:** `app.py`, `functions.py`, `pod_num_peak.py`, `curl.py`, `trigger_go/`
- **What it tests:** sustained **request throughput** and behaviour as the number of function
  pods/replicas grows (`pod_num_peak.py` finds the peak concurrent pod count). `peak-mem.sh`
  (one level up) pairs with this to capture **peak memory** under load.
- **Metric:** requests/sec at scale, peak pod count, peak memory.

---

## RTSFaaS (MorphStream FaaS) — `RTSFaaS/scripts/FaaS/MicroBenchmark`

### `MicroBenchmark/` — synthetic transactional workload
- **Driver:** `driver.sh`, `worker.sh` (Java client class `client.$DAGName` from `morph-clients`)
- **What it tests:** a **single-table counter** transactional workload used to stress the
  concurrency-control / lease-transfer machinery in isolation. Key knobs exposed in `driver.sh`:
  - `numberItemsForTables = 80000` keys, value type String, value size 8 B
  - `ratioOfMultiPartitionTransactionsForEvents` — **% of cross-partition (distributed) txns**
  - `stateAccessSkewnessForEvents` — **access skew / contention** (hot-key behaviour)
  - `abortRatioForEvents` — induced **abort rate**
  - `CCOption=3` (TSTREAM), `isRDMA=1`, tunable worker/thread/frontend/batch counts
- **Metric:** transactional throughput / latency as a function of contention, skew, multi-partition
  ratio, and worker count — i.e. how well the lease-based concurrency control holds up under stress.
- **Why not copied:** it is a synthetic knob-sweep, not an application; the real apps
  (**MediaReview**, **SocialNetwork**) were copied instead.

---

## Cloudburst — `cloudburst/cloudburst/server/benchmarks/`

These share the dispatch in `server.py` (routes by name). Copied apps: `predserving`, `mobilenet`,
`summa`. Documented micro-benchmarks:

| File | What it tests |
|------|---------------|
| `composition.py` | **Function-composition latency.** Registers a trivial 2-stage DAG (`incr → square`) and times `call_dag` end-to-end, splitting **scheduler time vs KVS time**. Pure orchestration overhead with negligible compute. |
| `locality.py` | **Data locality / cache effectiveness.** A `dot`-product DAG over ten ~1 MB objects (`OSIZE=1000000`) stored in the KVS, referenced via `CloudburstReference`, to measure the benefit of executing near cached state vs fetching it. |
| `lambda_locality.py` | Same locality idea against **external stores (`redis`, `s3`)** — measures data-fetch latency when state lives outside the co-located Anna cache. |
| `scaling.py` | **Autoscaling behaviour.** A `slp` (sleep) DAG driving many concurrent requests to observe how the system adds/removes executors and how latency responds under load. |
| `centr_avg.py` | **Centralized averaging** micro-benchmark — baseline where averaging happens at a single node. |
| `dist_avg.py` | **Distributed averaging** micro-benchmark — gossip/lattice-based averaging across nodes; paired with `centr_avg` to contrast centralized vs distributed aggregation cost. |

**Metric (all):** end-to-end latency distributions (and scheduler/KVS breakdown), throughput under
concurrency.

---

## Roadrunner — `roadrunner/experiments/motivation` and `.../evaluation`

Roadrunner is itself a data-transfer system, so most of its evaluation is micro-benchmark-shaped.
The runnable example apps (`image-resize`, `fanout-wasm`, `fanout-container`) were copied; the
measurement harnesses are documented here.

### `experiments/motivation/` — native/container vs Wasm overhead
- **Scripts:** `run_{example}.sh`, `run_wasm_{example}.sh`, `parallel_run.sh`, `parallel_run_wasm.sh`
- **What it tests:** end-to-end **latency and throughput** of the same function run as a native/runc
  container vs a WasmEdge container (`io.containerd.runc.v2` vs `io.containerd.wasmedge.v1`), single
  and parallel. Establishes the WASI inter-function-communication overhead Roadrunner targets.
- **Results:** `motivation-{wasmedge,container}.csv`, `transfer-{wasmedge,container}.csv`.

### `experiments/evaluation/` — transfer-mode micro-benchmark
- **Scripts:** `roadrunner-embedded.sh`, `roadrunner-kernel-mode.sh`, `roadrunner-net-mode.sh`,
  `intra-inter-node-{container,wasmedge}.sh`
- **What it tests:** the core claim — **data-delivery cost across transfer mechanisms**:
  - user-space (WasmEdge linear-memory APIs), kernel-space (UNIX sockets), network (`splice`/`vmsplice`)
  - **intra-node vs inter-node**, sequential vs parallel **fanout**
  - **payload sizes 1 MB → 500 MB** (`input-data/make-payloads.sh` → `file_1M.txt … file_500M.txt`)
  - a small Rust/warp **HTTP file server** serves payloads to isolate transfer/serialization cost
    from compute (the "HTTP storage transfer" baseline).
- **Metric:** transfer latency/throughput and copy count per mode, scaling with payload size and
  fanout degree.

---

## Faasm — ATC '20 evaluation (nothing to copy locally)

The local `faasm/` checkout contains only unit/dist **tests** (`tests/`), not the paper workloads —
and it is the later GRANNY-era codebase, not the ATC '20 evaluation code. None of Faasm's workloads
could be copied. See `benchmarks/Faasm/README.md` for the full per-§ breakdown and headline results;
the items below are the parts of the ATC '20 evaluation that are *micro-benchmark*-shaped.

| Workload (paper §) | Category | What it measures |
|--------------------|----------|------------------|
| **Polybench/C** (§6.4b) | **Micro-benchmark** | ~22 compute kernels compiled to WebAssembly — per-kernel runtime overhead vs. native, isolating the Wasm/Faaslet execution cost from any state movement. |
| **Python Performance Benchmarks** (§6.4c) | **Micro-benchmark** | ~23 CPython benchmarks run inside a Faaslet vs. native CPython — dynamic-language runtime overhead. |
| **Faaslet vs. container cold-start** (§6.5) | **Micro-benchmark** | No-op init: init time, CPU cycles, PSS/RSS footprint, and host capacity for Docker vs. Faaslets vs. Proto-Faaslets (plus a Python no-op restoring a pre-initialised CPython snapshot). |

The remaining ATC '20 experiments are **application workloads**, not micro-benchmarks: distributed
**ML training** (HOGWILD! SGD on Reuters RCV1, §6.2), **ML inference** (TensorFlow Lite + MobileNet,
§6.3), and divide-and-conquer **matrix multiplication** (Python + NumPy, §6.4a). They would belong in
the copied-workloads set, but none are checked out locally.

> Note: the GRANNY-era `faasm/experiment-*` repos referenced in `faasm/docs/index.rst`
> (MPI/LAMMPS, OpenMP/CovidSim+LULESH, SGX) are a *newer, different* HPC workload set — **not** the
> ATC '20 experiments — and are likewise not present in this tree.
