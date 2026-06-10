# Faasm workloads — present in the 2020 (ATC '20) tree

**Paper:** ATC '20 — *Faasm: Lightweight Isolation for Efficient Stateful Serverless Computing*
(Shillaker & Pietzuch, Imperial College London). [arXiv:2002.09344](https://arxiv.org/abs/2002.09344)

The Faasm paper workloads **are** shipped in-tree, but only on the **ATC '20-era tags** of the
`faasm/` checkout. They live under `faasm/func/` (plus supporting library/test code). The local
`faasm/` checkout has been switched to **`v0.2.4`** (tagged 2020-07-15, the ATC '20 presentation
window), so the workload code is physically present on disk.

> **Note on versions.** The newer **GRANNY-era** `main` (v0.33.0) **deletes `func/` entirely** —
> there the workloads were moved out to external `faasm/experiment-*` repos (MPI/LAMMPS,
> OpenMP/CovidSim+LULESH, SGX), which are a *different*, newer set of HPC workloads and are **not**
> the ATC '20 experiments. If you check `main` you will see only the runtime and its unit/dist
> tests (`faasm/tests/`) and conclude the workloads are missing — they are not missing, they are on
> the 2020 tags. To restore them: `cd faasm && git checkout v0.2.4`.

## ATC '20 evaluation (paper §6)

- **Baseline:** Knative (container-based, on Kubernetes), running the *same* code via a
  Knative-specific implementation of the Faaslet host interface; Redis as the distributed KVS.
- **Testbed:** 20 hosts (Intel Xeon E3-1220 3.1 GHz, 16 GB RAM, 1 Gbps); the cold-start/footprint
  experiments ran on a single Xeon E5-2660 (32 GB).
- **Key metric:** *billable memory* (GB-seconds = peak memory × runtime × #functions), plus
  execution time, throughput and latency.

| § | Workload | Measures | Dataset / model | Code in `faasm/` (@ v0.2.4) |
|---|----------|----------|-----------------|-----------------------------|
| 6.2 | **ML training** — distributed SGD via the **HOGWILD!** algorithm, text classification | Training time, network transfer, billable memory vs. parallelism (2→38 functions) | **Reuters RCV1** (800K examples; 128-example run to isolate overheads) | `func/sgd/reuters_svm.cpp`, `libs/cpp/sgd.cpp`, `libs/cpp/faasm/sgd.h`, `tests/test/lib/test_sgd.cpp`; prebuilt `wasm/sgd/reuters_svm/function.wasm` |
| 6.3 | **ML inference** — **TensorFlow Lite** image classification | Throughput vs. median latency, latency CDF, cold-start ratio (0% / 2% / 20%) | Pre-trained **MobileNet**; images from a file server | `func/tf/` (`image.cc`, `bitmap_helpers.cc`, `get_top_n*.h`) + bundled model `func/tf/data/mobilenet_v1_1.0_224.tflite`, `grace_hopper.bmp`, `labels.txt` |
| 6.4a | **Matrix multiplication** — divide-and-conquer Python + NumPy (CPython in a Faaslet) | Duration + network transfer vs. matrix size | Up to 8000×8000; 64 multiply + 9 merge functions via recursive chaining | `func/demo/matrix.cpp`, `func/python/mat_mul.py` |
| 6.4b | **Polybench/C** compute microbenchmarks, compiled to WebAssembly | Per-kernel overhead vs. native | ~22 Polybench kernels | `func/polybench/` (~31 `.c` kernels across datamining/linear-algebra/medley/stencils); `bin/build_polybench_native.sh` |
| 6.4c | **Python Performance Benchmarks** under CPython | Per-benchmark overhead vs. native | ~23 Python benchmarks | `func/python/bench_*.py` (24 files) |
| 6.5 | **Faaslets vs. containers** — no-op cold start | Init time, CPU cycles, PSS/RSS footprint, host capacity | Docker vs. Faaslet vs. Proto-Faaslet; plus a Python no-op restoring a pre-initialised CPython snapshot | `func/demo/noop.c`, `func/python/noop.py` |

**Headline results:** ~2× faster ML training with ~10× less memory; ~2× inference throughput with
~90% lower tail latency; ~5,600× faster init and ~15× smaller footprint than Docker.

## Copied here (application workloads)

Following the same convention as the other four systems, the **application workloads** were copied
into this folder; the **micro-benchmarks** are documented in
[`../MICROBENCHMARKS.md`](../MICROBENCHMARKS.md) and not copied.

```
Faasm/
├── ml-training-sgd/     §6.2  HOGWILD! SGD text classifier (Reuters RCV1)
│   ├── reuters_svm.cpp        the Faaslet function
│   ├── CMakeLists.txt
│   └── lib/                   faasm-facing SGD library (sgd.cpp, faasm/sgd.h)
├── tflite-inference/    §6.3  TensorFlow Lite image classification (MobileNet)
│   ├── image.cc, bitmap_helpers.cc, get_top_n*.h, image.h, CMakeLists.txt
│   ├── data/                  bundled model + sample image + labels
│   │   ├── mobilenet_v1_1.0_224.tflite
│   │   ├── grace_hopper.bmp
│   │   └── labels.txt
│   └── driver/                host-side helpers
│       ├── tensorflow.py      faasmcli task: upload model state + tfdata
│       ├── tflite_bench.lua   nginx/openresty load-gen config (throughput/latency)
│       └── build_tflite_native.sh
└── matrix-multiply/     §6.4a  divide-and-conquer matrix multiply (recursive chaining)
    ├── matrix.cpp             the Faaslet function (func/demo/matrix.cpp)
    ├── mat_mul.py             CPython/NumPy variant (func/python/mat_mul.py)
    └── lib/                   faasm-facing matrix library (matrix.cpp, faasm/matrix.h)
```

**Not copied (micro-benchmarks — documented in `../MICROBENCHMARKS.md`):**
§6.4b Polybench/C (`faasm/func/polybench/`, ~31 kernels), §6.4c Python Performance Benchmarks
(`faasm/func/python/bench_*.py`, 24 files), §6.5 cold-start no-op (`faasm/func/demo/noop.c`,
`faasm/func/python/noop.py`). These isolate per-kernel / per-benchmark / init overhead rather than
running a realistic application.

### Dependencies (these were lifted out of the repo)

Like the other copies, the workloads need their original context to actually build/run:

- All three are **Faaslet functions** built with the Faasm WASM toolchain against the runtime in
  `../../faasm` (the `faasm/` C++ libs, host interface, and `faasmcli` upload/call tasks). Build
  them from the source tree, not from these copies in isolation.
- **matrix-multiply** also depends on `SparseMatrixFileSerialiser` (`faasm/include/storage/` +
  `faasm/src/storage/`) and the recursive-chaining host calls — runtime internals, not copied here.
- **tflite-inference** needs the model/data uploaded as Faasm state via `driver/tensorflow.py`
  (`faasm-cli tensorflow.state` / `tensorflow.upload`) before the function can run.

### Where this came from

The code is on disk because the local `faasm/` checkout is at tag **`v0.2.4`** (2020-07-15, the
ATC '20 window), where these workloads still ship in-tree under `faasm/func/`. The newer GRANNY-era
`main` (v0.33.0) deletes `func/` and moves workloads to external `experiment-*` repos. To restore
or browse the source: `cd ../../faasm && git checkout v0.2.4` (use `git checkout main` to go back).
