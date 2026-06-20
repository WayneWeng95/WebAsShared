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

## TeraSort note (the all-to-all shuffle)

TeraSort is an N×N transpose, not a fan→1 gather. **`--fanout == --nodes`** (the
driver's default) places one partition worker + one range-owner per node, so each
directed node-pair carries exactly one stream — the deadlock-safe layout. `N > nodes`
puts several owners per node (>1 transfer per machine-pair); prefer `N == nodes` for
first bring-up.
