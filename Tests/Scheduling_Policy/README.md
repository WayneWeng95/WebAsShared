# Scheduling_Policy — placement policy vs. input size (word_count)

Sweeps the partitioner's **placement policy** — `pack`, `balanced`, `spread`,
`random` — against **input corpus size** for the **word_count** workload on a
**4-node** cluster, and compares it to a multi-node **Faasm-like** baseline.

Node convention: node-0 = `10.10.1.2` (coordinator), node-1 = `10.10.1.1`,
node-2 = `10.10.1.3`, node-3 = `10.10.1.4` (see `NodeAgent/agent.toml`).

> **Scope note.** This experiment currently covers **word_count only**.
> finra and ml_training are deferred: auto-placing their parallel fan under a
> *distributing* policy makes the executor deadlock at runtime in the cross-node
> many-to-many RDMA gather (a `placement:"all"` per-machine aggregator pulling
> from a scattered fan). `pack` is the only policy that runs them today. See
> "Status / deferred work" below.

---

## What "policy" means

The policy lives in the Partitioner (`Partitioner/partitioner/src/policies.rs`),
selected per-DAG via `placement_policy`:

| policy     | rule                                                          |
|------------|---------------------------------------------------------------|
| `pack`     | concentrate auto-placed nodes on the single most-capable host |
| `balanced` | spread proportionally across all hosts                        |
| `spread`   | at most one auto-placed node per host                         |
| `random`   | each auto-placed node to a uniformly random host              |

### word_count is the policy-INSENSITIVE control

word_count's heavy stages use `placement:"all"` (the partitioner replicates them
on *every* host regardless of policy), and its only auto-placed nodes — the
convergence tail — are pinned to node 0 by `converge_on_coordinator`. So all four
policies place the work **identically**. Confirmed earlier: at 500 MB the four
policies landed within ~2.6 % of each other (8070–8282 ms).

That makes word_count a clean **control**: sweeping input size shows
(a) how the system scales with corpus size, and (b) that placement policy does
**not** move latency at any size. Any divergence across policies here would
indicate noise or a real placement effect leaking in — useful either way.

(Why offline pre-partitioning is still used: on a live cluster the coordinator's
SCX hints would override the embedded policy — `splitter.rs`,
`hints.or(policy_hints)`. Pre-partitioning with the `partition` binary keeps the
policy authoritative and the cluster_dag is passed through unchanged.)

---

## Files

| file                 | role                                                           |
|----------------------|----------------------------------------------------------------|
| `wc_size_sweep.sh`   | the experiment: (size × policy) → pre-partition, submit, time → `results_wc_size.csv` |
| `gen_variants.py`    | emit a policy variant of a workload; `--input PATH` overrides corpus size |
| `placement_stats.py` | static placement metrics (imbalance, cross-node edges) of a ClusterDag |

Generated (gitignored): `cluster_dags/`, `logs_wc_size/`.
`gen_variants.py` still carries finra/ml_training support for the deferred work.

---

## Run

Prerequisites: cluster up (coordinator + 3 workers), corpora on **node-0**:
`TestData/corpus_large.txt` (50 MB), `corpus_xlarge.txt` (500 MB),
`corpus.txt` (1 GB). The coordinator RDMA-stages them to the workers.

```bash
cd Tests/Scheduling_Policy
./wc_size_sweep.sh 3                       # 3 sizes × 4 policies × 3 reps (median)
# subset:
SIZES="corpus_large.txt corpus_xlarge.txt" POLICIES="pack balanced" ./wc_size_sweep.sh 5
```

Output `results_wc_size.csv`:

```
size_mb,corpus,policy,wall_ms_median,wall_ms_min,wall_ms_max,maxcompute_ms_median,throughput_mb_s,success,reps
```

- `wall_ms_*`     end-to-end latency (coordinator-reported "total wall time", median of N)
- `maxcompute_ms` slowest per-node compute (straggler) from the job Summary
- `throughput_mb_s` = size_mb / (wall_ms/1000)

Expected shape: latency grows ~linearly with size; the four policies overlap at
every size (the control holding across the size axis).

---

## Baseline — multi-node Faasm-like word_count (TO BUILD)

To make the numbers comparable we need a baseline that runs the **same**
map-reduce word_count across the **same** 4 nodes, but with a different system's
data-movement model. **Reuse the existing Faasm-like WordCount baseline** — don't
write a new one — at:

```
Tests/Inter-Node Application_Benchmark/WordCount/baseline/faasm/demo/
  wc.rs / wc.cwasm   the Faaslet WASM module (map + reduce), AOT-compiled
  driver.py          orchestrates Faaslets, moves state through Redis KV
  run.sh             sweeps size × fan-out N → results.csv
```

That baseline already models the Faasm mechanism we want to contrast: each
map/reduce stage is a fresh `wasmtime` Faaslet running `wc.cwasm`, and the driver
ships corpus chunks + partial counts through **Redis KV** (`chunk_<i>` → mappers →
`partial_<i>` → reducer). The serialized KV blobs are exactly the inter-stage
transfer our zero-copy RDMA/SHM page-chain avoids. It already sweeps the **same**
corpus sizes {50, 500, 1000 MB} and emits the same CSV column shape — so its rows
line up against `results_wc_size.csv` directly.

**Only change needed to make it multi-node** (reusing `wc.cwasm` + `driver.py`
unchanged as far as possible):

1. **One shared Redis on node-0** (`redis-server --bind 0.0.0.0`); point every
   node's driver at `REDIS_HOST=10.10.1.2`. The KV transfer now crosses the
   network — the realistic distributed-Faasm behaviour (state shipped through the
   store between nodes), vs. our RDMA path. `driver.py` already takes
   `REDIS_HOST/PORT`, so this is config, not code.
2. **Place mapper Faaslets across the 4 nodes** instead of all-local: have the
   driver launch mapper `i` on node `i % 4` (the only real code change — a remote
   `wasmtime` launch; SSH/`node-agent` exec, whichever is already wired). Mapper
   still reads its `chunk_<i>` and writes `partial_<i>` through the shared Redis,
   so its logic is untouched. Splitter/reducer stay on node-0.
3. **Same axes, same columns:** corpus size {50, 500, 1000 MB} × fan-out N. Faasm
   has no placement-policy analogue — that's the point: it's the "no placement
   control" reference our four policies are measured against.

Deliverable: extend the demo's `run.sh` with a `NODES=`/remote-dispatch mode (or a
thin wrapper next to it) emitting `results_baseline.csv` in the shared shape — then
plot ours-vs-Faasm latency and throughput against corpus size. The headline
contrast: our page-chain moves state zero-copy between nodes; Faasm serializes it
through the KV, so the gap should widen with corpus size.

---

## Status / deferred work

- ✅ **Partitioner cycle fixed.** Distributing policies used to emit a *cyclic*
  per-node DAG (rejected at run time with "DAG contains a cycle"). Fixed in
  `splitter.rs` (the wave-0 deadlock-prevention pass now updates its reachability
  map incrementally); regression test `sandwich_placement_is_acyclic`.
- ⏳ **finra / ml_training distributing policies** still **deadlock at runtime**
  in the executor's cross-node many-to-many gather (separate, deeper issue in
  `Executor/host`). Only `pack` runs them. Revisit when extending past
  word_count.
- ⏳ **Coordinator robustness:** a hung job currently crashes the coordinator
  rather than timing out cleanly — worth hardening before long unattended sweeps.
