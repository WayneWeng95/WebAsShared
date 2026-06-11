# Cluster_Eval — evaluating the recent changes

Two test cases for the recent work (locality `split` hint, partitioner placer,
and the executor/type changes from Mode 2 + streaming fan-out). The first is
local; the second runs across the real 2-node cluster.

Node convention (same as `Tests/Streaming_CrossNode/`): node-0 = `10.10.1.2`,
node-1 = `10.10.1.1`, device `mlx4_0`.

---

## 1. Locality `split` hint — structural (LOCAL, no cluster)

Proves the per-node `split` hint deterministically controls **where the
partitioner breaks the topology**: an asymmetric join (`heavy`/`light` → `join`)
is partitioned, and the cross-machine cut must land on the edge marked
`split: "prefer"`, flipping when the hints are reversed.

```bash
python3 Tests/Cluster_Eval/check_locality.py
```
Expected: `PASS — split hint controls the cut`
(hinted → cut on LIGHT edge; reversed → cut FLIPS to HEAVY edge).

---

## 2. Cross-node word-count — end-to-end (CLUSTER)

Regression for the full **partition → RDMA input staging → distributed execute →
cross-node aggregate → reduce** path after all the changes, AND it exercises the
partitioner placer (locality hint) against the live cluster.

### 2a. Proper flow — via the node-agent (auto RDMA input staging)  ← recommended

The **coordinator** RDMA-stages `shared_inputs` to workers
(`coordinator.rs::stage_shared_inputs_rdma`, TCP fallback), so the corpus only
needs to live on **node-0** — it is RDMA-written to node-1 automatically. The
coordinator also partitions the SymbolicDag against the live cluster (running the
placer + `split` hint) and collects results.

Prerequisites: same commit + `./build.sh` on both nodes; `corpus_large.txt` on
**node-0 only** (the coordinator/owner).

```bash
# node-0 (10.10.1.2) — coordinator:
./node-agent start --config NodeAgent/agent.toml          # auto-detects node 0 = coordinator
# node-1 (10.10.1.1) — worker:
./node-agent start --config NodeAgent/agent.toml          # auto-detects node 1 = worker

# submit the SymbolicDag (from node-0 / any host with coordinator access):
./node-agent submit --config NodeAgent/agent.toml \
    --dag DAGs/symbolic_dag/word_count_auto_placement.json
```
The coordinator partitions, RDMA-stages the corpus to node-1, runs, and collects
the word-count result (the `save`/Output node; converges to the coordinator).
Re-submit a few times to confirm determinism. To evaluate the **locality hint**
end-to-end, submit a SymbolicDag carrying `split` hints and inspect the placement
the coordinator logs.

> NOTE: this is the path that does input distribution. Bare `host dag` (§2b) does
> NOT stage inputs — it only sets up the RDMA mesh for `RemoteSend`/`RemoteRecv`
> between compute stages, so each node must already have its input locally.

### 2b. Lower-level flow — bare `host dag` (no node-agent; manual input)

Direct executor path (like `Tests/Streaming_CrossNode/`). No coordinator, so
**no input staging** — the corpus must be on BOTH nodes (`scp` it, or use a small
committed corpus). Useful to test the executor without the daemon stack.

```bash
# baseline (node-0, once):
Partitioner/target/release/partition Tests/Cluster_Eval/wc_xnode.json --nodes 1 \
  | python3 -c "import json,sys; c=json.load(sys.stdin); \
      json.dump({'shm_path':'/dev/shm/ce_base','wasm_path':'Executor/target/wasm32-unknown-unknown/release/guest.wasm','log_level':'error','nodes':c['node_dags']['0']}, open('/tmp/ce_base.json','w'))"
Executor/target/release/host dag /tmp/ce_base.json && cp TestOutput/cluster_eval_result.txt /tmp/baseline_counts.txt

# per-node DAGs (node-0); copy out/node_1.json to node-1 (same path):
python3 Tests/Cluster_Eval/make_xnode_dags.py Tests/Cluster_Eval/wc_xnode.json \
  --ips 10.10.1.2,10.10.1.1 --out Tests/Cluster_Eval/out

# run (node-1 first, then node-0):
Executor/target/release/host dag Tests/Cluster_Eval/out/node_1.json   # on 10.10.1.1
Executor/target/release/host dag Tests/Cluster_Eval/out/node_0.json   # on 10.10.1.2

# verify (node-0):
python3 Tests/Cluster_Eval/verify.py /tmp/baseline_counts.txt TestOutput/cluster_eval_result.txt
```
Local loopback smoke test (one machine, real RDMA `mlx4_0`): same as above but
`--ips 127.0.0.1,127.0.0.1` and run both processes locally (node-1 first). ✅
Verified here: 2-node loopback result == single-node baseline.

---

## What each test covers
| Test | Exercises | Status |
|------|-----------|--------|
| `check_locality.py` | partitioner placer + `split`/`out_weight` hint → cut placement | ✅ PASS (local) |
| word-count via node-agent (§2a) | RDMA input staging + partition + distributed exec + aggregate + reduce | run on cluster |
| word-count bare host dag (§2b) | executor distributed path (no staging) | ✅ loopback PASS (== baseline) |
