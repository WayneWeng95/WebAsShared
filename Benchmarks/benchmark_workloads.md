# Benchmark Workloads Across the 5 Comparison Systems

This document summarizes the benchmark workloads used by each of the five serverless/FaaS
systems under `/opt/myapp/compare_system`. Information was gathered from each project's
`README`, experiment/benchmark directories, run scripts, and documentation.

| System | Publication / Origin | Type / Domain | Benchmark workloads used |
|--------|---------------------|---------------|--------------------------|
| **RMMap** (dmerge) | EuroSys '24 — *Serialization/Deserialization-free State Transfer in Serverless Workflows with RDMA-based Remote Memory Map* | DAG serverless workflows + micro | • **MNIST / digital-minist** (handwritten-digit ML inference)<br>• **FINRA** (financial compliance workflow)<br>• **ML pipeline** (multi-stage ML workflow)<br>• **WordCount** (C++ and Java variants)<br>• **Micro-benchmarks** (`exp/micro`, throughput `exp/thpt`, peak-mem) |
| **RTSFaaS** (MorphStream FaaS branch) | RDMA-capable transactional stateful FaaS framework | Transactional stateful applications + micro | • **MediaReview** (transactional app)<br>• **SocialNetwork** (transactional app)<br>• **MicroBenchmark** (synthetic transactional workload) |
| **Cloudburst** (Hydro project) | VLDB '20 — stateful serverless on the Anna KVS | Function compositions + ML serving | • **composition** (function-composition latency)<br>• **locality** / **lambda_locality** (`redis`, `s3` data locality)<br>• **predserving** (prediction-serving pipeline)<br>• **mobilenet** (image-classification inference)<br>• **scaling** (autoscaling)<br>• **summa** (distributed matrix multiplication)<br>• **centr_avg / dist_avg** (centralized vs distributed averaging) |
| **Faasm** | ATC '20 — *Faasm: Lightweight Isolation for Efficient Stateful Serverless Computing* | WebAssembly stateful serverless; ML + dynamic-language runtime + cold-start | • **ML training** — distributed SGD (HOGWILD!) text classification on **Reuters RCV1**<br>• **ML inference** — **TensorFlow Lite** + **MobileNet** image classification<br>• **Matrix multiplication** — divide-and-conquer NumPy / CPython<br>• **Polybench/C** compute microbenchmarks (compiled to Wasm)<br>• **Python Performance Benchmarks** (CPython in a Faaslet)<br>• **Faaslet vs. container cold-start** (no-op init time / footprint) |
| **Roadrunner** | Middleware '25 — *Accelerating Data Delivery to WebAssembly-Based Serverless Functions* | Data-transfer / inter-function communication | • **Fanout** (Wasm fanout + container fanout; intra-/inter-node, sequential & parallel) with **file payloads of 1M–500M**<br>• **Image-resize** (motivation micro-benchmark)<br>• **HTTP storage transfer** (pure transfer/serialization overhead via warp file server) |

---

## Per-System Details

### 1. RMMap (dmerge)
- **Location:** `RMMap/exp/`
- **Workload directories:** `digital-minist/`, `finra/`, `ml-pipeline/`, `wordcount/`, `wordcount-java/`, `micro/`, `thpt/` (throughput)
- **Platform:** Knative on a 3-node cluster (master + 2 workers), RDMA + custom kernel module.
- **Focus:** Serialization/deserialization-free state transfer between functions in DAG workflows.
- **Notes:** `digital-minist` is MNIST digit recognition; `finra` is a FINRA financial-compliance
  workflow; `ml-pipeline` is a multi-stage ML workflow. `peak-mem.sh` measures peak memory.

### 2. RTSFaaS (MorphStream FaaS branch)
- **Location:** `RTSFaaS/scripts/FaaS/{MediaReview,SocialNetwork,MicroBenchmark}/` (driver/worker scripts)
- **Workload config:** `scripts/FaaS/Env/{MediaReview.env, SocialNetwork.env}`
- **Application code:** `morph-clients/src/main/java/benchmark/{rdma,socket}`
- **Platform:** 5-machine cluster, RDMA (Mellanox ConnectX-3), Docker containers, TiKV key-value store.
- **Focus:** RDMA-capable transactional stateful workflows with lease-based concurrency control.

### 3. Cloudburst
- **Location:** `cloudburst/cloudburst/server/benchmarks/`
- **Benchmark files:** `composition.py`, `locality.py`, `lambda_locality.py`, `predserving.py`,
  `mobilenet.py`, `scaling.py`, `summa.py`, `centr_avg.py`, `dist_avg.py`
- **Dispatch:** `server.py` routes by name → `composition`, `locality`, `redis`, `s3`,
  `predserving`, `mobilenet`, `scaling`
- **Platform:** Built on Anna KVS + Anna cache; local or cluster mode.
- **Focus:** Low-latency stateful function compositions and ML prediction serving.

### 4. Faasm
- **Paper:** ATC '20 — *Faasm: Lightweight Isolation for Efficient Stateful Serverless Computing*
  (Shillaker & Pietzuch, Imperial College London). [arXiv:2002.09344](https://arxiv.org/abs/2002.09344)
- **Location:** The paper workloads are **not** shipped in this tree. The local `faasm/` checkout
  carries only unit/dist tests (`faasm/tests/`); it is the later GRANNY-era codebase whose
  `docs/index.rst` points at external `faasm/experiment-*` repos. The original ATC '20 evaluation
  code is not present locally.
- **Baseline:** **Knative** (container-based, on Kubernetes), running the *same* code via a
  Knative-specific implementation of the Faaslet host interface; Redis as the distributed KVS.
- **Testbed:** 20 hosts (Intel Xeon E3-1220 3.1 GHz, 16 GB RAM, 1 Gbps); cold-start/footprint
  experiments on a single Xeon E5-2660 (32 GB).
- **Key metric:** *billable memory* (GB-seconds = peak memory × runtime × #functions), alongside
  execution time, throughput and latency.
- **Workloads (paper §6):**
  - **§6.2 ML training** — distributed SGD via the **HOGWILD!** algorithm, text classification on the
    **Reuters RCV1** dataset (800K examples, scaled 2→38 parallel functions). Measures training time,
    network transfer, billable memory.
  - **§6.3 ML inference** — **TensorFlow Lite** image classification with a pre-trained **MobileNet**
    model, images from a file server. Measures throughput vs. latency and latency CDF under varying
    cold-start ratios (0% / 2% / 20%).
  - **§6.4 Language-runtime performance (Python):** (a) distributed divide-and-conquer **matrix
    multiplication** in Python + NumPy (CPython in a Faaslet; 64 multiply + 9 merge functions via
    recursive chaining, up to 8000×8000); (b) **Polybench/C** compiled to WebAssembly (compute
    microbenchmarks vs. native); (c) **Python Performance Benchmarks** under CPython (vs. native).
  - **§6.5 Faaslets vs. containers** — no-op cold starts: init time, CPU cycles, PSS/RSS footprint,
    host capacity (Docker vs. Faaslets vs. Proto-Faaslets); plus a Python no-op restoring a
    pre-initialised CPython snapshot.
- **Platform:** WebAssembly (WAVM) runtime with Faaslet SFI isolation + Linux cgroups; Proto-Faaslet
  snapshots for fast cold starts.
- **Focus:** Lightweight isolation and shared-memory state management for stateful serverless;
  efficient parallel ML and low-latency inference vs. containers.
- **Headline results:** ~2× faster ML training with ~10× less memory; ~2× inference throughput with
  ~90% lower tail latency; ~5,600× faster init and ~15× smaller footprint than Docker.

### 5. Roadrunner
- **Location:** `roadrunner/experiments/evaluation/` and `roadrunner/experiments/motivation/`
- **Examples:** `examples/{fanout-wasm, fanout-container, image-resize, hello-world, wasm-server, wasm-client, alice-wasm-lib}`
- **Evaluation scripts:** `experiments/evaluation/scripts/` — `roadrunner-embedded.sh`,
  `roadrunner-kernel-mode.sh`, `roadrunner-net-mode.sh`, `intra-inter-node-{container,wasmedge}.sh`
- **Payloads:** `file_1M.txt` … `file_500M.txt` (generated by `input-data/make-payloads.sh`);
  served by a small Rust/warp HTTP file server to isolate transfer/serialization cost from compute.
- **Platform:** Rust + WasmEdge runtime; user-space, kernel-space (UNIX sockets), and network
  (`splice`/`vmsplice`) transfer modes.
- **Focus:** Serialization-free, minimal-copy data delivery between Wasm functions.

---

## Cross-System Observations
- **ML inference** appears in three systems: RMMap (MNIST), Cloudburst (MobileNet), Faasm (TensorFlow Lite + MobileNet).
- **Word/data processing & micro-benchmarks** appear in nearly all five — the natural common axes for comparison.
- **RMMap, RTSFaaS, Cloudburst, Roadrunner** ship workloads inside the repo; **Faasm's** ATC '20 evaluation code is not present locally (the local checkout is the later GRANNY-era codebase).
- **RMMap, RTSFaaS, and Roadrunner** target serialization/data-transfer overhead; **Cloudburst and Faasm** target stateful computation more broadly.
