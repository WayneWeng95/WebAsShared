# Kubernetes + Knative setup — inter-node baseline frameworks

Stands up the shared cluster the inter-node **baselines** run on (tracks B & C of
`../EXPERIMENT_PLAN.md`):

| Framework | Runtime | Where it deploys |
|-----------|---------|------------------|
| **Cloudburst** | Kubernetes (executors/scheduler as pods) | this k3s cluster (`cloudburst/`) |
| **RMMap / dmerge** | Knative (`Service` + `Sequence`/`Parallel`) | Knative Serving on this cluster (`knative/`) |

We use **k3s** (lightweight k8s: bundles containerd + flannel CNI + service LB, so
cross-node pods and LoadBalancer Services work with no extra setup) and **Knative
Serving + Kourier** (no Istio). It runs on the same 4 boxes as WasMem
(node 0 = control-plane, nodes 1-3 = workers); **run experiments one at a time** —
k8s steals CPU/RAM from a concurrent WasMem run (fairness rule §5.4).

## Files

| File | Run on | Purpose |
|------|--------|---------|
| `deploy-all.sh`          | **any node** | **one command** — stands up the whole stack (role-aware) |
| `cluster.env`            | — | topology + versions + `REDIS_HOST` (edit first) |
| `01-install-server.sh`   | node 0 | install k3s control-plane; prints the worker join command |
| `02-install-agent.sh`    | each worker | join the node to the cluster |
| `03-install-knative.sh`  | node 0 | Knative Serving + Kourier + **Eventing** + default broker — **RMMap** runtime |
| `06-strimzi.sh`          | node 0 | Strimzi operator + Kafka + topics — **Flink StateFun** runtime |
| `07-load-images.sh`      | each node | distribute StateFun's custom images to every node's containerd |
| `05-redis.sh`            | node 0 | in-cluster Redis (**Cloudburst/RMMap** KV; testing — real runs use the dedicated box) |
| `04-verify.sh`           | node 0 | smoke test: nodes Ready, cross-node networking, a Knative hello-world |
| `statefun/statefun-kafka.yaml` | — | StateFun Kafka cluster + 10 topics (modern Strimzi v1beta2) |

This covers every **k8s-based** framework: **Cloudburst** (k8s) · **RMMap** (Knative)
· **Flink StateFun** (Strimzi/Kafka), plus the shared **Redis**. (Faasm uses a per-node
agent and RTSFaaS uses ssh+TiKV — separate, not k8s.)

## One-command deploy (recommended)

```bash
cd "Tests/Inter-Node deployment/k8s" && vim cluster.env   # set REDIS_HOST etc.
chmod +x *.sh

# On node 0 — brings up EVERYTHING (k3s + Knative + Strimzi/Kafka + Redis + images
# + verify). If passwordless SSH to workers exists it joins + seeds them too;
# otherwise it prints the one command to run on each worker:
./deploy-all.sh
```

If SSH between nodes is unavailable (it is publickey-blocked here), copy the
generated `.cluster-join` to each worker and run `./deploy-all.sh` there — it
auto-detects the worker role, joins, and imports the StateFun images.

## Manual / step-by-step (what deploy-all.sh runs under the hood)

```bash
# 0. Edit topology/versions/Redis once (git-synced to every node):
vim "Tests/Inter-Node deployment/k8s/cluster.env"
cd  "Tests/Inter-Node deployment/k8s" && chmod +x *.sh

# 1. CONTROL-PLANE — on node 0 (10.10.1.2):
./01-install-server.sh
#    → copy the "K3S_URL=… K3S_TOKEN=… ./02-install-agent.sh" line it prints.

# 2. WORKERS — run on EACH of nodes 1-3 (SSH between nodes is publickey-blocked
#    here, so run it locally on each, like the rtsfaas setup):
K3S_URL=https://10.10.1.2:6443 K3S_TOKEN='<token-from-step-1>' ./02-install-agent.sh

# 3. KNATIVE — back on node 0, once all workers show Ready:
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
kubectl get nodes -o wide          # confirm 4 Ready
./03-install-knative.sh

# 4. VERIFY — on node 0:
./04-verify.sh                     # → "CLUSTER READY ✓"

# 5. (optional) quick Redis for a baseline smoke test:
./05-redis.sh
```

`kubectl` on node 0: `export KUBECONFIG=/etc/rancher/k3s/k3s.yaml` (or use `k3s kubectl`).

## Deploying the baselines onto it (next step, per EXPERIMENT_PLAN §3)

- **Cloudburst → `cloudburst/`**: executors + scheduler as Deployments/pods across
  the nodes; point its KV at `REDIS_HOST`. Reuse the per-stage function code in
  `../../Intra-Node Application_Benchmark/<W>/baseline/cloudburst/`.
- **RMMap → `knative/`**: one Knative `Service` per stage wired into a
  `Sequence`/`Parallel` flow; state via Redis (**ES protocol only — no MITOSIS
  kernel module** without explicit authorization). Reuse
  `../../Intra-Node Application_Benchmark/<W>/baseline/rmmap/`.
- **Flink StateFun (streaming baseline) → `statefun/`**: the Kafka env is created by
  `06-strimzi.sh`. Then (a) build the 3 `streaming-example-*` images and distribute
  them with `07-load-images.sh` (export on node 0 → import on every node), (b) set
  `imagePullPolicy: IfNotPresent` and apply the master + 2 function deployments from
  `../../Streaming Application_Benchmark/intra-node/baseline/FlinkStateFun/deployment/k8s/`
  (`flink-statefun.yaml`, `mediareview-function.yaml`, `socialnetwork-function.yaml`)
  — skip their `kafka.yaml`/`topics.yaml` (ours supersede them), (c) scale
  `statefun-worker` replicas + `parallelism.default` across nodes, (d) drive with
  `stream_bench.py batch` from a producer pod. Bootstrap:
  `kafka-cluster-kafka-bootstrap.default.svc:9092`.
- Measurement boundary stays **data-path only** (compute + state transfer); the
  k8s/Knative control-plane overhead is excluded, same window as intra-node
  (EXPERIMENT_PLAN §5.9).

## Scaling to 9 nodes

Add the 5 new IPs to `AGENT_IPS` in `cluster.env`, run `02-install-agent.sh` on
each new box. No control-plane change; re-run `04-verify.sh`.

## Teardown

```bash
# workers:        /usr/local/bin/k3s-agent-uninstall.sh
# control-plane:  /usr/local/bin/k3s-uninstall.sh
```

## Troubleshooting

- **Worker won't join / cross-node ping fails** — open the firewall: TCP `6443`
  (API), UDP `8472` (flannel VXLAN), TCP `10250` (kubelet). Set `CLUSTER_IFACE` in
  `cluster.env` if the default route isn't the 10.10.1.x NIC.
- **`ksvc` stuck not-Ready / image pull errors** — the nodes need to reach
  `gcr.io/knative-releases` and `gcr.io/knative-samples`. See *Air-gapped* below.
- **Kourier external IP `<pending>`** — k3s servicelb needs a free node IP;
  `kubectl -n kourier-system get svc kourier`. You can also `kubectl port-forward`.
- **Air-gapped cluster** — the install scripts pull from `get.k3s.io` and GitHub
  releases. Offline: pre-stage the k3s binary + airgap images (k3s airgap tarball)
  and `kubectl apply` locally-downloaded `serving-*.yaml`/`kourier.yaml`, and mirror
  the Knative images into a local registry. The scripts otherwise work unchanged.
