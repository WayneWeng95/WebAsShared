# Faasm workloads — not present in this tree

Unlike the other four systems, Faasm does **not** ship its paper workloads inside the main repo.
The local `faasm/` checkout carries only unit/dist tests (`faasm/tests/`), not the evaluation
workloads. The real workloads live in separate `experiment-*` repositories referenced from
`faasm/docs/index.rst`:

| Repo (external) | Category | Workloads |
|-----------------|----------|-----------|
| `experiment-microbench` | Micro-benchmark | C++ and Python micro-benchmarks (see `../MICROBENCHMARKS.md`) |
| `experiment-mpi` | Application (HPC) | MPI: LAMMPS (molecular dynamics) + ParRes Kernels |
| `experiment-openmp` | Application (HPC) | OpenMP: CovidSim + LULESH + ParRes Kernels |
| `experiment-sgx` | Application | SGX image-processing pipeline |

Nothing was copied here because none of these are checked out locally. To obtain them, clone the
corresponding `faasm/experiment-*` repos.
