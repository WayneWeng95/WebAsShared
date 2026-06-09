# Benchmark Workloads Across the 5 Comparison Systems

This document summarizes the benchmark workloads used by each of the five serverless/FaaS
systems under `/opt/myapp/compare_system`. Information was gathered from each project's
`README`, experiment/benchmark directories, run scripts, and documentation.

| System | Publication / Origin | Type / Domain | Benchmark workloads used |
|--------|---------------------|---------------|--------------------------|
| **RMMap** (dmerge) | EuroSys '24 — *Serialization/Deserialization-free State Transfer in Serverless Workflows with RDMA-based Remote Memory Map* | DAG serverless workflows + micro | • **MNIST / digital-minist** (handwritten-digit ML inference)<br>• **FINRA** (financial compliance workflow)<br>• **ML pipeline** (multi-stage ML workflow)<br>• **WordCount** (C++ and Java variants)<br>• **Micro-benchmarks** (`exp/micro`, throughput `exp/thpt`, peak-mem) |
| **RTSFaaS** (MorphStream FaaS branch) | RDMA-capable transactional stateful FaaS framework | Transactional stateful applications + micro | • **MediaReview** (transactional app)<br>• **SocialNetwork** (transactional app)<br>• **MicroBenchmark** (synthetic transactional workload) |
| **Cloudburst** (Hydro project) | VLDB '20 — stateful serverless on the Anna KVS | Function compositions + ML serving | • **composition** (function-composition latency)<br>• **locality** / **lambda_locality** (`redis`, `s3` data locality)<br>• **predserving** (prediction-serving pipeline)<br>• **mobilenet** (image-classification inference)<br>• **scaling** (autoscaling)<br>• **summa** (distributed matrix multiplication)<br>• **centr_avg / dist_avg** (centralized vs distributed averaging) |
| **Faasm** | ATC '20 (Faasm) / NSDI '25 (GRANNY) — WebAssembly stateful serverless | HPC / MPI / OpenMP / ML | • **Microbenchmarks** (C++ and Python)<br>• **MPI:** LAMMPS (molecular dynamics) + ParRes Kernels<br>• **OpenMP:** CovidSim + LULESH + ParRes Kernels<br>• **SGX:** image-processing pipeline |
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
- **Location:** Workloads live in **separate** `faasm/experiment-*` repos, referenced in `faasm/docs/index.rst`.
  The main repo carries unit/dist tests (`tests/`), not the paper workloads.
- **Experiment repos:**
  - `experiment-microbench` — Microbenchmarks (C++ and Python)
  - `experiment-mpi` — MPI: LAMMPS + ParRes Kernels
  - `experiment-openmp` — OpenMP: CovidSim + LULESH + ParRes Kernels
  - `experiment-sgx` — SGX: image-processing pipeline
- **Platform:** WebAssembly (WAVM/WAMR) runtime, supports MPI, OpenMP, pthreads, SGX.
- **Focus:** High-performance stateful serverless with HPC-style parallel workloads.

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
- **ML inference** appears in three systems: RMMap (MNIST), Cloudburst (MobileNet), Faasm (SGX image pipeline).
- **Word/data processing & micro-benchmarks** appear in nearly all five — the natural common axes for comparison.
- **RMMap, RTSFaaS, Cloudburst, Roadrunner** ship workloads inside the repo; **Faasm** keeps them in external `experiment-*` repos.
- **RMMap, RTSFaaS, and Roadrunner** target serialization/data-transfer overhead; **Cloudburst and Faasm** target stateful computation more broadly.
