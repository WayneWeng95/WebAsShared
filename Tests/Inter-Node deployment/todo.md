# Todo — Inter-Node Deployment (shared infra for app + streaming benchmarks)

Detailed plan: [`EXPERIMENT_PLAN.md`](EXPERIMENT_PLAN.md). This is the actionable
checklist. The two seeds:

- **K8s-based frameworks → deploy Kubernetes.** (Cloudburst [app], Flink StateFun
  [streaming]; RMMap layers Knative on top.)
- **Non-serverless frameworks → a small per-node Python agent for collaboration.**
  (Faasm [clean-slate]; also the generic fallback runner for any baseline we can't
  stand up on its real framework.)

## Deployment tracks (see plan §1 for the framework→category table)
- [x] **(A) WasMem** — `node-agent submit`; runs both benchmarks inter-node already.
- [ ] **(B) Kubernetes cluster** (node0 control-plane + node1-3 workers; k3s or kubeadm)
  - [ ] Flink StateFun (manifests exist → deploy + scale workers/parallelism)
  - [ ] Cloudburst (executors/scheduler as pods; KV → Redis box)
- [ ] **(C) Knative on k8s** — RMMap Service+Sequence/Parallel flows; state via Redis.
- [ ] **(D) Per-node Python agent** (`agent/agent.py` + `agent/coordinator.py`)
  - [ ] launch / status / stop / stage / metrics endpoints
  - [ ] Faasm runner integration (launch Faaslets per node; KV → Redis box)
  - [ ] emit `results.csv` (shared column shape) + per-node memory samples
- [~] **(E) RTSFaaS** — multi-node test scripted in [`rtsfaas/`](rtsfaas/). `setup-node.sh` self-installs per git-synced node (rdma_ucm + Java/Maven + DB patch + build jar/image, idempotent). **No-ssh path (recommended):** edit `cluster.env`, `bash setup-node.sh` on each node, then `./run-role.sh {database,driver,worker N,client}` one per node (coordinate via git-synced config + RDMA, no ssh). **ssh path:** `SETUP=1 ./run-rtsfaas-cluster.sh` automates it. In-memory store, no TiKV yet. TODO: run it; then `isTiKV=1` + TiUP for the design-point.

## Shared cluster prep (do first — plan §2, §7 phase 0)
- [ ] `sudo modprobe rdma_ucm` on every node (per boot).
- [ ] Passwordless ssh between nodes for this account (RTSFaaS + agent bring-up).
- [ ] Record each node's RoCE IP + reverse-DNS hostname; services-box IP.
- [ ] Stand up the **dedicated Redis box** (5th machine); set `REDIS_HOST` everywhere.
- [ ] (StateFun) Kafka via Strimzi; (RTSFaaS) TiKV via TiUP.

## Order (lowest risk first, plan §7)
1. cluster prep + Redis box → 2. k8s cluster → 3. StateFun on k8s →
4. per-node agent + Faasm → 5. Cloudburst (k8s) + RMMap (Knative) →
6. RTSFaaS + TiKV → 7. sweeps + figures (overlay intra-node style).

## Key gotchas (learned)
- Workers need inputs **staged**, not assumed shared-FS (agent `/stage`, StateFun `shared_inputs`, k8s PVC/configmap).
- RDMA: `rdma_ucm` per boot; match the NIC's **reverse-DNS hostname**, not the IP / not 127.0.0.1.
- Stand up the per-node agent **early** — it's also the documented fallback if a real framework can't be brought up in time.
