# Todo — Inter-Node Streaming Application

Actionable checklist for the cluster version. Design rationale + metrics +
fairness live in [`EXPERIMENT_PLAN.md`](EXPERIMENT_PLAN.md); this is the "what to
do" list. Lift from the done single-machine version in [`../intra-node/`](../intra-node/).

## 0. Cluster prep (all nodes)
- [ ] `sudo modprobe rdma_ucm` on every node; record each node's RoCE IP + reverse-DNS name (the `getHostName()` match gotcha — see `../intra-node/baseline/RTSFaaS/single_node/README.md`).
- [ ] Docker on all nodes; Strimzi Kafka operator on the k8s cluster.
- [ ] TiKV cluster up via TiUP (needed only for the RTSFaaS design-point run).

## 1. Flink StateFun on k8s (lowest risk — manifests already exist)
- [ ] Deploy `../intra-node/baseline/FlinkStateFun/deployment/k8s/*` on the cluster.
- [ ] Scale `statefun-worker` replicas + `parallelism.default` across nodes; scale the 2 function deployments.
- [ ] Run `stream_bench.py batch` (events/s) for both workloads from a producer pod; gate passes.
- [ ] FT stance per plan §5 (checkpointing off + heap backend, or labeled handicap).

## 2. RTSFaaS on the cluster (its real 5-node design point)
- [ ] `Env/Cluster.env`: real driver + N worker hosts (cluster RoCE names).
- [ ] `Env/Database.env`: `isRemoteDB=1, isTiKV=1`; point at the TiKV cluster.
- [ ] Deploy 4 roles over ssh via `scripts/FaaS/run-application.sh` (reuse the built `rtfaas:1.0` image + the single-node fixes; drop the in-memory `database` role when using TiKV).
- [ ] Run both workloads at scale; record client req/s + p50/p99.

## 3. WasMem on the cluster
### 3a. First version — data-parallel + RDMA merge (READY to run)
- [x] `gen_cluster_dag.py` — emits the submit ClusterDag (`node_dags` format, matches `DAGs/cluster_dag/word_count.json`): each node loads a disjoint event slice → seed → parse → apply×n → aggregate_local → non-mergers `RemoteSend`→merger; merger `RemoteRecv`+aggregate_global+`*_summary`+Output. Reuses the intra-node guest unchanged (no rebuild).
- [x] `run-cluster.sh` — gen events → round-robin split into N disjoint slices → gen DAG → `node-agent submit --config agent_coordinator.toml`. Locally validated (DAG shape, disjoint+complete split, JSON).
- [x] **Ran on the cluster at 2 AND 4 nodes, gate PASSES** for both workloads — matches the intra-node gate exactly (MediaReview `total_events=20000, login_ok=4007, rate=8150, review=7843`; SocialNetwork `login_ok=5096, profile=5075, timeline=9758, tweet=9900`). At 4 nodes the merger (node 3) correctly merges the slices received from nodes 0/1/2. Two fixes were needed: (1) emit `shared_inputs` so the coordinator stages per-node slices to workers (without it the worker's `load` fails → success=false); (2) the merger's `RemoteRecv` must sit at the SAME wave depth as the senders' `RemoteSend` (`deps=[aggregate_local]`, not `[parse]`) or the cross-node rendezvous silently drops the sender's half.
- [ ] Throughput (events/s) + FT via the `Persist` offload per node (async + sync-barrier, already in `../intra-node/gen_dag.py`).

### 3b. Richer version — key-owning RDMA routing (the headline; follow-up)
- [ ] Cross-node DAG where each key range is owned by a node and a remote key's **state access** crosses the network via the RDMA page-chain (`DAGs/rdma_workload_dag/` or a `--cluster --key-owning` mode). Drives the multi-partition-ratio x-axis. May need a `parse` variant that filters/forwards by owning node (guest change → `./build.sh`).

## 4. Sweeps
- [ ] **multi-partition ratio** (`gen_events.py --multipartition`) = cross-node state-access ratio — the headline x-axis.
- [ ] **skew** (`--skew`) = hot-node imbalance.
- [ ] **node count** scale-out.
- [ ] Per run: req/s, p50/p99, **cross-node bytes** (RDMA vs Kafka/TiKV), per-node peak memory.

## 5. Analysis / figures
- [ ] `results.csv` (cols in plan §4); `plot.py` reusing `../intra-node/plot.py` style (broken two-stage axis, muted palette) so figures overlay with intra-node.
- [ ] Headline table + cross-node-bytes figure (the inter-node headline: RDMA page-chain ≈ 0 serialization vs Kafka/TiKV network bytes).

## Open decisions
- [ ] FT for all systems on inter-node? (recommend off for the throughput line + a separate WasMem FT-on bar, as intra-node.)
- [ ] Same N across systems (RTSFaaS single-node capped at 16k — re-check on cluster).
- [ ] Node/core budget to match across the three.
