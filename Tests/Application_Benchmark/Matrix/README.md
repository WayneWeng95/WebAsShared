# Matrix multiplication — WebAsShared vs RMMap / Faasm / Cloudburst  *(backlog)*

Workload **#4** in [`../../../Benchmarks/workload_selection_table.tex`](../../../Benchmarks/workload_selection_table.tex).
Divide-and-conquer dense matrix multiply via recursive function chaining — maps onto our fan-out /
aggregate routing and stresses cross-node block transfer over RDMA.

Detailed when WordCount (#2) is complete. Sketch:

- **Our side (port needed):** a guest workload that tiles A·B into block multiplies fanned out across
  workers and aggregated — built on our fan-out + `Aggregate` routing primitives. Large block
  transfers showcase the serialization-free RDMA path.
- **Faasm (primary, native):** `Benchmarks/Faasm/matrix-multiply/` (`matrix.cpp`, `mat_mul.py`,
  `lib/`) — WASM divide-and-conquer with recursive host-call chaining. WASM-vs-WASM head-to-head.
- **Cloudburst (native, SUMMA):** `Benchmarks/Cloudburst/summa.py` — 2D block algorithm; runs over
  Redis (not Anna — see caveat).
- **RMMap (port needed):** express the block multiply as a DMERGE DAG to get a second RDMA point.
- **Metrics:** GFLOP/s, e2e runtime, cross-node bytes (RDMA), peak/billable memory, serialization
  cost of block transfer. See [`../README.md`](../README.md).
