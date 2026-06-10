# FINRA — WebAsShared vs RMMap / Faasm / Cloudburst  *(backlog)*

Workload **#3** in [`../../../Benchmarks/workload_selection_table.tex`](../../../Benchmarks/workload_selection_table.tex).
Multi-stage financial-compliance audit workflow that passes intermediate state between stages — the
inter-stage state movement that page-chain splicing targets.

Detailed when WordCount (#2) is complete. Sketch:

- **Our side (native):** `Executor/guest/src/workloads/finra.rs`. Express as a multi-stage DAG
  (`Input → fetch → compute stages → Output`) under `DAGs/`. Reuse, don't modify.
- **RMMap (primary, native):** base functions `Benchmarks/RMMap/finra/` (`functions.py`,
  `full-flow.py`, `service.yaml`); same `PROTOCOL` ∈ {`DMERGE`, `ES`, …} transport selection as
  WordCount. Match input (`yfinance.csv` / `data_fetcher.py`).
- **Faasm (port needed):** chained Faaslets, per-stage state in Faasm KV.
- **Cloudburst (port needed):** Python DAG of the stages over Redis.
- **Metrics:** per-stage exec time, inter-stage state-transfer cost (serialization = 0 on our side),
  e2e runtime, peak/billable memory, RDMA bytes. See [`../README.md`](../README.md) for the common
  set and the Redis caveat.
