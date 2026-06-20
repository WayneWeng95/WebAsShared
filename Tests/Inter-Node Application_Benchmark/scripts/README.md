# Inter-Node Application Benchmark — WasMem generation scripts

Self-contained copy of the WasMem cluster-execution generators for **all six**
benchmark workloads. The Inter-Node benchmark targets **makespan**, so every
workload is generated with the **balanced** placement policy (the makespan-favouring
distributing policy) — there is **no policy sweep** here. (The policy-sweep study is
the separate, completed `Tests/Scheduling_Policy/` experiment; these files were
copied out of it so this benchmark stands alone and that folder stays frozen.)

## Files

| Script | Role |
|--------|------|
| `gen_variants.py`        | Main generator. `policy` defaults to **balanced**. Workloads: `word_count, finra, ml_training, ml_inference, matrix, terasort`. |
| `gen_sgd_ap_dag.py`      | Parametric ML-training (SGD) fan generator (`--fanout`). |
| `gen_inference_ap_dag.py`| Parametric MNIST-inference fan generator (`--fanout`). |
| `gen_matrix_ap_dag.py`   | Parametric SUMMA block-grid generator (`--fanout` = r·c, `--matrix-n` = N). |
| `gen_terasort_ap_dag.py` | All-to-all shuffle (transpose) generator (`--fanout` = partition/owner count). |
| `gen_dags.sh`            | Driver: generate + offline-partition the balanced ClusterDag for all six. |

`word_count` and `finra` need no separate generator — `gen_variants.py` reads their
static `DAGs/symbolic_dag/<wl>_auto_placement.json` and applies the fan transform
(finra's hybrid sharding) inline.

## Pipeline

```
gen_variants.py <wl> --nodes N [--fanout W] [...] --out <sym>   # policy defaults to balanced
partition <sym> --nodes N > <cdag>                              # OFFLINE — embedded placement authoritative
node-agent submit --config NodeAgent/agent.toml --dag <cdag>    # on the cluster
```

Pre-partition **offline** so the embedded balanced placement is authoritative (a
live cluster's SCX hints would otherwise override it).

## Quick start

```bash
# From the repo root, after ./build.sh:
Tests/Inter-Node\ Application_Benchmark/scripts/gen_dags.sh 4     # 4-node shakeout
NODES=9 Tests/Inter-Node\ Application_Benchmark/scripts/gen_dags.sh   # 9-node full run
```

Produces `scripts/dags/<wl>.cdag.json`, ready for `node-agent submit`.

## Inputs to stage on every node

| Workload | Input path(s) |
|----------|---------------|
| word_count   | `TestData/corpus_xlarge.txt` |
| finra        | `TestData/trades.csv` |
| ml_training  | `TestData/ml/sgd_100000.csv` |
| ml_inference | `TestData/mnist/model.csv`, `TestData/mnist/test.csv` |
| matrix       | `TestData/matrix/A_<N>.bin`, `TestData/matrix/B_<N>.bin` |
| terasort     | `TestData/terasort/records_*.txt` |

## Verifying correctness on the cluster

`./verify.sh` runs each workload as ground-truth (`--nodes 1`) and distributed
(`--nodes N`) and asserts the fan-out-invariant gate matches. Cluster-verified
2026-06-20 (4 nodes): ml_inference `prediction_checksum=155371`, matrix
`checksum=2722562338`, terasort `records=335544 keysum=216366291 sorted=1`.

```bash
Tests/Inter-Node\ Application_Benchmark/scripts/verify.sh                 # ml_inference matrix terasort
NODES=4 Tests/Inter-Node\ Application_Benchmark/scripts/verify.sh matrix  # one workload
```

## TeraSort note — CENTRALIZED gather, not all-to-all

TeraSort's true all-to-all transpose (every node ↔ every node) **deadlocks the
executor's blocking RDMA transport** — the partitioner can't order all sends before
all recvs without a cycle. So the multi-node path uses the proven UNIDIRECTIONAL
fan-gather instead: each worker routes its shard by owner (`ts_partition`), combines
into ONE stream, and sends ONE transfer to node 0, which gathers (one RemoteRecv per
peer) and sorts the dataset. The cross-node cost is the full shuffled dataset moving
to node 0 over RDMA; the merge is centralized (single reducer node). True all-to-all
would need executor-side non-blocking/phased transfers (see the plan §8). The two
guest fns this uses — `ts_range_summary`, `ts_finalize` — are additive; the
intra-node `ts_merge` + per-range outputs are untouched.
