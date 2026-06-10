# ML training — WebAsShared vs RMMap / Faasm / Cloudburst  *(backlog)*

Workload **#5** in [`../../../Benchmarks/workload_selection_table.tex`](../../../Benchmarks/workload_selection_table.tex).
Multi-stage distributed model training (SGD); RMMap's `ml-pipeline` is the primary baseline (our
closest twin: RDMA, serialization-free state transfer), with Faasm (HOGWILD! SGD) and a stretch
Cloudburst port as secondary baselines.

Detailed when WordCount (#2) is complete. Sketch:

- **Our side (native):** `Executor/guest/src/workloads/ml_training.rs`. Express the
  load → partition → per-worker SGD → aggregate(model) → … DAG under `DAGs/`. Reuse, don't modify.
- **RMMap (primary, native):** base functions `Benchmarks/RMMap/ml-pipeline/` (and
  `digital-minist/` for the model/data) — `PROTOCOL` ∈ {`DMERGE`, `ES`, …}; the per-iteration model
  gradients are the serialization-free state transfer.
- **Faasm (native, HOGWILD!):** `Benchmarks/Faasm/ml-training-sgd/` (`reuters_svm.cpp`) — shared-state
  parallel SGD; carries the **billable-memory / footprint** argument.
- **Cloudburst (port needed, stretch):** distributed SGD over Redis via its averaging primitives.
- **Metrics:** time-to-accuracy / per-epoch time, gradient/model transfer cost (ser = 0 on our
  side), peak/billable memory, RDMA bytes. See [`../README.md`](../README.md).
