# WasMem inter-node WordCount test

Our framework's counterpart to `../faasm/`, running the **same** WordCount work on the
same cluster and measured the **same way**, so the two `results.csv` files line up
column-for-column.

| File | Role |
|------|------|
| `run_wordcount.py` | node-0 driver: gen+partition (offline) → `node-agent submit` → parse makespan + per-node durations, gate occurrences, append `results.csv` |
| `run.sh` | size sweep at a fixed fan width (default 40 = 10 map workers/node × 4 nodes) |

## How the work maps to "10 jobs per machine"

`word_count` is the policy **control** in `gen_variants.py`, so its `--fanout` CLI knob
is intentionally inert. The map fan width lives in the DAG node `map_n.fanout`; the
driver sets it directly (default **40**) and uses the **balanced** policy. The offline
partitioner then spreads the fan evenly: `partition --nodes 4` →
`map_n_<node>_0..9` on each node = **10 `wc_map` workers per node**, each node loading
its own ¼ slice and converging to node 0's `reduce`→`save` via RDMA `RemoteRecv`.

## Measurement boundary (matches Faasm; partitioner excluded)

```
gen_variants/build_sym  ─┐
partition --nodes N      ─┴─ OFFLINE, UNTIMED (the scheduler's job)
node-agent submit  ───────── TIMED: coordinator job_start → all-complete
```

- **makespan** = the coordinator's `total wall time` (Job Summary). It is
  `job_start.elapsed()` measured *after* the offline partition and worker-DAG
  dispatch (`coordinator.rs`), so it is pure job execution — input RDMA-staging +
  compute + cross-node aggregate + reduce + the `save` Output node — with the
  **partitioner excluded**, exactly like the Faasm window excludes its (nonexistent)
  scheduler. The result is delivered (written by `save`) inside this window.
- **total_job_ms** = Σ per-node executor durations from the summary.

  ⚠️ **Granularity differs from Faasm.** Here there is **one executor per node** that
  runs that node's ~10 `wc_map` workers concurrently in-process, so `total_job_ms` ≈
  `nodes × (per-node wall)` ≈ a few × makespan. In Faasm each mapper is its own OS
  process and `total_job_ms` = Σ of all ~41 process wall times (so it scales with the
  worker *count* and its per-process startup). Compare **makespan** directly; treat
  `total_job_ms` as "aggregate node-busy time," not a like-for-like of Faasm's
  Σ-per-process.

## Prerequisites

1. Build: `./build.sh` (needs `Partitioner/target/release/partition` + `node-agent`).
2. Cluster up on **every** node: `./node-agent start --config NodeAgent/agent.toml`,
   then confirm with `./node-agent status --config NodeAgent/agent.toml`.
3. Corpora on **node 0 only** (they are the `shared_inputs` source, RDMA-staged to the
   ¼-slice owners): `TestData/corpus_large.txt` (50MB), `corpus_xlarge.txt` (500MB),
   `corpus_1gb.txt` (1GB).

## Run

```bash
./run.sh                                  # fanout 40, 3 reps, 50/500/1000 MB
python3 run_wordcount.py --corpus TestData/corpus_xlarge.txt --fanout 40 --reps 3
```

`results.csv` columns (identical to `../faasm/results.csv`):
`size_mb,mappers,nodes_used,makespan_ms_median,total_job_ms_median,occurrences,expect,success,reps`
(`mappers` = `map_n.fanout`).
