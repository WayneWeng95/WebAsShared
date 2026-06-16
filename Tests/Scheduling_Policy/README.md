# Scheduling_Policy — impact of placement policy on a 4-node cluster

Sweeps the partitioner's **placement policy** — `pack`, `balanced`, `spread`,
`random` — across three workloads (**word_count**, **finra**, **ml_training**) on
a **4-node** cluster, and measures how the placement decision moves end-to-end
latency and cross-node RDMA traffic.

Node convention (same as the other cluster tests): node-0 = `10.10.1.2`
(coordinator), node-1 = `10.10.1.1`, node-2 = `10.10.1.3`, node-3 = `10.10.1.4`,
as listed in `NodeAgent/agent.toml`.

---

## What "policy" means here

The policy lives in the **Partitioner** (`Partitioner/partitioner/src/policies.rs`)
and is selected per-DAG via the `placement_policy` field:

| policy     | rule                                                        |
|------------|-------------------------------------------------------------|
| `pack`     | concentrate auto-placed nodes on the single most-capable host |
| `balanced` | spread proportionally across all hosts                      |
| `spread`   | at most one auto-placed node per host                       |
| `random`   | each auto-placed node to a uniformly random host            |

(The coordinator's live SCX advisor — `NodeAgent/scheduler/` with the
`ADVISOR_W_*` weights in `NodeAgent/common/src/lib.rs` — is a *separate*, fifth
strategy. This experiment isolates the four named policies; see "Why offline"
below for how we keep the advisor from overriding them.)

---

## Two things you must know (they shaped this harness)

**1. The stock `_auto_placement` DAGs are policy-INSENSITIVE.**
Their heavy parallel stages use `placement: "all"`, so the partitioner replicates
them on *every* host no matter the policy; the only auto-placed nodes are the
convergence tail, which `converge_on_coordinator` pins to node 0. Result: all
four policies place work identically. To make the policy matter, `gen_variants.py`
**auto-places the parallel fan**:

- `finra`       → the 8 `rule_*` audit rules become auto-placed
- `ml_training` → the 8 `train_*` SGD shards become auto-placed
- `word_count`  → **left unchanged — it is the CONTROL.** Its `map_n` fan is welded
  to `wc_distribute`'s output-slot layout and cannot be auto-placed without
  restructuring. Seeing it stay flat while the other two move *is a result*:
  workloads dominated by `placement:"all"` stages don't respond to placement
  policy.

**2. Why offline pre-partitioning.**
On a live cluster the coordinator hands the partitioner SCX-derived capacity
hints, and `hints.or(policy_hints)` (`splitter.rs:142`) means those hints
**override** the embedded `placement_policy`. So we partition **offline** with the
`partition` binary (no `--hints`), which makes the policy authoritative, and
submit the finished `ClusterDag`. `resolve_dag()` sees a pre-partitioned DAG and
passes it through unchanged — the policy is preserved exactly.

---

## Files

| file                 | role                                                       |
|----------------------|------------------------------------------------------------|
| `gen_variants.py`    | emit a policy-sensitive SymbolicDag for one (workload,policy) |
| `placement_stats.py` | static decision metrics (imbalance, cross-node edges) of a ClusterDag |
| `prepartition.sh`    | generate + offline-partition all 12 cells → `cluster_dags/`, write `placement.csv` |
| `run_sweep.sh`       | submit each ClusterDag to the live cluster, time it → `results.csv` |
| `analyze.py`         | join decision + latency → `summary.csv` and a per-workload table |

Generated (gitignored): `variants/`, `cluster_dags/`, `logs/`,
`placement.csv`, `results.csv`, `summary.csv`.

---

## Run it

### Step 1 — pre-partition (LOCAL, no cluster needed)

```bash
cd Tests/Scheduling_Policy
./prepartition.sh 4         # 4 = node count
```

Writes `cluster_dags/<wl>__<policy>.json` and `placement.csv`. You can inspect the
scheduling decisions immediately:

```bash
python3 analyze.py          # placement columns only, latency blank
```

Expected (decision metrics are deterministic):

```
=== finra ===
policy     busy  imbal  edges compute/host
pack          4     10     60 14|4|4|4      ← concentrated, fewest RDMA edges
spread        4      6     78 11|5|5|5
random        4      4     80 9|5|6|6
balanced      4      1     82 6|7|7|6        ← even, most RDMA edges
```

word_count stays `6|4|4|4` across all four policies — the control.

### Step 2 — measure on the cluster

Prerequisites (do these yourself):

1. Same commit + `./build.sh` on all 4 nodes.
2. Start the agent on every node (identical command — role auto-detected):
   ```bash
   # node-0 (10.10.1.2, coordinator), node-1, node-2, node-3:
   ./node-agent start --config NodeAgent/agent.toml
   ```
3. Stage each workload's input on **node-0** (the coordinator RDMA-stages
   `shared_inputs` to the workers). The DAGs read, e.g.,
   `TestData/corpus_xlarge.txt` (word_count) and the finra / ml_training datasets
   under `Benchmarks/RMMap/`.

Then sweep:

```bash
cd Tests/Scheduling_Policy
./run_sweep.sh 5                       # 5 repeats/cell, median reported
# subset:
WORKLOADS="finra ml_training" POLICIES="pack balanced" ./run_sweep.sh 5
```

`run_sweep.sh` submits each pre-partitioned ClusterDag, measures end-to-end
wall-clock (submit → JobResult), and writes `results.csv`. Per-run agent output is
kept in `logs/` (grep for `[DAG][timing] TOTAL compute` there if you want
compute-only timing instead of wall-clock).

### Step 3 — analyze

```bash
python3 analyze.py
```

Joins `placement.csv` + `results.csv` into `summary.csv` and prints, per workload,
the policies sorted by latency plus the headline `balanced vs pack` delta.

---

## Metrics

| column             | meaning                                                      |
|--------------------|--------------------------------------------------------------|
| `busy_hosts`       | hosts that received ≥1 compute sandbox                       |
| `imbalance`        | max−min compute sandboxes across hosts (0 = perfectly even)  |
| `cross_node_edges` | RemoteSend/RemoteRecv endpoints = cross-host RDMA wiring     |
| `compute_per_host` | compute sandboxes on host 0\|1\|2\|3                         |
| `wall_ms_median`   | end-to-end latency (submit → result), median of N repeats    |

**The story:** `pack` minimizes cross-node edges but piles work on one host (high
imbalance → CPU bottleneck); `balanced` evens the load but pays maximal RDMA
traffic; `spread`/`random` sit between. Which one wins on latency depends on
whether the workload is CPU-bound (favors `balanced`) or network-bound (favors
`pack`) — that trade-off is exactly what the cluster run reveals.

## Tweaking further

- **Different policy mix / weights:** edit `POLICIES` in the scripts, or add a
  `weighted` cell — `gen_variants.py` can be extended to emit
  `{"type":"weighted","weights":[...]}`.
- **The live advisor instead of static policies:** change the `ADVISOR_W_*`
  weights in `NodeAgent/common/src/lib.rs`, rebuild, and submit the *raw*
  SymbolicDags (not the pre-partitioned ones) so the coordinator's hints apply.
- **Node count:** `./prepartition.sh N` and update `NodeAgent/agent.toml` IPs.
