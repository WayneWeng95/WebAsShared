# Faasm workloads — not present in this tree

**Paper:** ATC '20 — *Faasm: Lightweight Isolation for Efficient Stateful Serverless Computing*
(Shillaker & Pietzuch, Imperial College London). [arXiv:2002.09344](https://arxiv.org/abs/2002.09344)

Unlike the other four systems, the Faasm evaluation workloads are **not** shipped in this tree.
The local `faasm/` checkout carries only unit/dist tests (`faasm/tests/`), not the paper workloads.
That checkout is the later **GRANNY-era** codebase whose `docs/index.rst` points at external
`faasm/experiment-*` repos (MPI/LAMMPS, OpenMP/CovidSim+LULESH, SGX) — those are a *different*,
newer set of HPC workloads and are **not** the experiments from the ATC '20 paper.

## ATC '20 evaluation (paper §6)

- **Baseline:** Knative (container-based, on Kubernetes), running the *same* code via a
  Knative-specific implementation of the Faaslet host interface; Redis as the distributed KVS.
- **Testbed:** 20 hosts (Intel Xeon E3-1220 3.1 GHz, 16 GB RAM, 1 Gbps); the cold-start/footprint
  experiments ran on a single Xeon E5-2660 (32 GB).
- **Key metric:** *billable memory* (GB-seconds = peak memory × runtime × #functions), plus
  execution time, throughput and latency.

| § | Workload | Measures | Dataset / model |
|---|----------|----------|-----------------|
| 6.2 | **ML training** — distributed SGD via the **HOGWILD!** algorithm, text classification | Training time, network transfer, billable memory vs. parallelism (2→38 functions) | **Reuters RCV1** (800K examples; 128-example run to isolate overheads) |
| 6.3 | **ML inference** — **TensorFlow Lite** image classification | Throughput vs. median latency, latency CDF, cold-start ratio (0% / 2% / 20%) | Pre-trained **MobileNet**; images from a file server |
| 6.4a | **Matrix multiplication** — divide-and-conquer Python + NumPy (CPython in a Faaslet) | Duration + network transfer vs. matrix size | Up to 8000×8000; 64 multiply + 9 merge functions via recursive chaining |
| 6.4b | **Polybench/C** compute microbenchmarks, compiled to WebAssembly | Per-kernel overhead vs. native | ~22 Polybench kernels |
| 6.4c | **Python Performance Benchmarks** under CPython | Per-benchmark overhead vs. native | ~23 Python benchmarks |
| 6.5 | **Faaslets vs. containers** — no-op cold start | Init time, CPU cycles, PSS/RSS footprint, host capacity | Docker vs. Faaslet vs. Proto-Faaslet; plus a Python no-op restoring a pre-initialised CPython snapshot |

**Headline results:** ~2× faster ML training with ~10× less memory; ~2× inference throughput with
~90% lower tail latency; ~5,600× faster init and ~15× smaller footprint than Docker.

## Why nothing is copied here

None of the ATC '20 evaluation code is checked out locally — the local `faasm/` repo only contains
the runtime and its unit/dist tests. The newer GRANNY-era `faasm/experiment-*` repos referenced in
`faasm/docs/index.rst` are not the ATC '20 workloads and are also not checked out (they are not git
submodules). To obtain workloads, clone the relevant upstream repositories separately.
