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

Regression for the full **partition → RDMA → distributed execute → cross-node
aggregate → reduce** path after all the changes. `wc_xnode.json` is
`word_count_auto_placement` on the 50 MB `corpus_large.txt`; `placement:"all"`
runs map work on both nodes and the per-node partials are RDMA-aggregated on
node-0. The distributed result must equal a single-node run of the same corpus.

### Prerequisites (BOTH nodes)
- Same commit + `./build.sh` on **both** nodes (the conn-3/4 rebuild gotcha —
  a stale binary deadlocks the mesh).
- `TestData/corpus_large.txt` present on both nodes.

### Step A — single-node baseline (on node-0, once)
```bash
Partitioner/target/release/partition Tests/Cluster_Eval/wc_xnode.json --nodes 1 \
  | python3 -c "import json,sys; c=json.load(sys.stdin); \
      json.dump({'shm_path':'/dev/shm/ce_base','wasm_path':'Executor/target/wasm32-unknown-unknown/release/guest.wasm','log_level':'error','nodes':c['node_dags']['0']}, open('/tmp/ce_base.json','w'))"
Executor/target/release/host dag /tmp/ce_base.json
cp TestOutput/cluster_eval_result.txt /tmp/baseline_counts.txt
```

### Step B — generate per-node DAGs (on node-0)
```bash
python3 Tests/Cluster_Eval/make_xnode_dags.py Tests/Cluster_Eval/wc_xnode.json \
  --ips 10.10.1.2,10.10.1.1 --out Tests/Cluster_Eval/out
```
Copy `Tests/Cluster_Eval/out/node_1.json` to node-1 (same path). `save` (Output)
lands on node-0, so the result file appears there.

### Step C — run (start both close together; mesh blocks until both are up)
```bash
# node-1 (10.10.1.1):
Executor/target/release/host dag Tests/Cluster_Eval/out/node_1.json
# node-0 (10.10.1.2):
Executor/target/release/host dag Tests/Cluster_Eval/out/node_0.json
```

### Step D — verify (on node-0)
```bash
python3 Tests/Cluster_Eval/verify.py /tmp/baseline_counts.txt TestOutput/cluster_eval_result.txt
```
Expected: `PASS — identical`. Re-run Step C a few times to confirm determinism.

### Local smoke test (optional, single machine)
Validates the harness before using cluster time (both nodes on loopback; needs a
local RDMA device — `mlx4_0` works):
```bash
python3 Tests/Cluster_Eval/make_xnode_dags.py Tests/Cluster_Eval/wc_xnode.json \
  --ips 127.0.0.1,127.0.0.1 --out /tmp/xn_wc
Executor/target/release/host dag /tmp/xn_wc/node_1.json &   # start first
Executor/target/release/host dag /tmp/xn_wc/node_0.json
python3 Tests/Cluster_Eval/verify.py /tmp/baseline_counts.txt TestOutput/cluster_eval_result.txt
```

---

## What each test covers
| Test | Exercises | Verified locally |
|------|-----------|------------------|
| `check_locality.py` | partitioner placer + `split`/`out_weight` hint → cut placement | ✅ PASS |
| cross-node word-count | partition → RDMA → distributed exec → aggregate → reduce, after all type/executor/partitioner changes | ✅ loopback PASS (== baseline) |
