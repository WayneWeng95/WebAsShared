# Inter-Node Baseline Frameworks — Kubernetes Deployment Runbook

End-to-end guide to stand up the shared **Kubernetes environment** that the
k8s-based inter-node baselines run on, and to deploy each framework's apps onto it.
All scripts/manifests referenced live in the **`k8s/`** subfolder of this directory.

| Framework | Runtime stood up here | Cross-node state |
|-----------|-----------------------|------------------|
| **Cloudburst** | Kubernetes (executors/scheduler as pods) | Redis |
| **RMMap / dmerge** | Knative Serving + **Eventing** (`Service` + CloudEvent broker) | Redis (ES protocol) |
| **Flink StateFun** | Strimzi + Kafka on Kubernetes | Kafka (protobuf/HTTP) |

> Faasm (per-node agent) and RTSFaaS (ssh + TiKV — see `rtsfaas/`) are **not** k8s —
> separate tracks. WasMem (ours) runs on its own `node-agent` and is already verified.

Stack: **k3s** (bundles containerd + flannel CNI + service LB → cross-node pods and
LoadBalancers work with no extra setup) · **Knative Serving + Kourier** (no Istio) ·
**Strimzi** (Kafka operator) · **Redis**.

---

## 1. Topology

Mirrors the WasMem cluster (`NodeAgent/agent.toml`): node 0 = control-plane,
nodes 1-3 = workers (scale to 9 for the full run). k3s coexists with the WasMem
node-agent on the same boxes (different ports) — but **run experiments one at a
time** (fairness rule §5.4): a live k8s control-plane + pods steal CPU/RAM from a
concurrent WasMem run.

```
node 0  10.10.1.2   k3s control-plane  (+ Knative/Strimzi/Redis control)
node 1  10.10.1.1   k3s worker
node 2  10.10.1.3   k3s worker
node 3  10.10.1.4   k3s worker
(Redis: dedicated 5th box off the compute path for real runs; in-cluster for smoke tests)
```

All settings live in **`k8s/cluster.env`** (git-synced to every node) — edit it
first: versions (k3s/Knative/Kourier/Strimzi), `SERVER_IP`/`AGENT_IPS`,
`REDIS_HOST`, `CLUSTER_IFACE` (the 10.10.1.x NIC is `eno1d1` on node 0), and the
StateFun image list.

---

## 2. Prerequisites

- `sudo` on every node; outbound HTTPS to `get.k3s.io`, GitHub releases,
  `gcr.io/knative-*`, `quay.io/strimzi`, Docker Hub (or a mirror — see §8 Air-gapped).
- Firewall between nodes open for: TCP **6443** (API), UDP **8472** (flannel VXLAN),
  TCP **10250** (kubelet); Kafka NodePort range if driving Kafka from outside.
- For Flink StateFun: the 3 `streaming-example-*` images **built** (in the streaming
  benchmark) on node 0 before image distribution.
- `chmod +x "Tests/Inter-Node deployment/k8s/"*.sh`.

---

## 3. Deploy — one command (recommended)

`k8s/deploy-all.sh` is **role-aware**: on the control-plane it builds the whole
stack; on a worker it joins + imports images.

```bash
cd "Tests/Inter-Node deployment/k8s"
vim cluster.env            # set REDIS_HOST, versions, etc.

# On node 0 — does EVERYTHING: k3s control-plane → join workers → Knative+Kourier
# → Strimzi+Kafka → Redis → StateFun image distribution → verify.
./deploy-all.sh
```

**SSH note:** passwordless SSH between nodes is publickey-blocked on this cluster,
so node 0 cannot reach into the workers. `deploy-all.sh` does the maximum in one
shot on node 0, writes the join creds to `k8s/.cluster-join` (gitignored), and
prints the worker command. On **each worker**, copy `.cluster-join` over (or
git-sync the folder) and run:

```bash
cd "Tests/Inter-Node deployment/k8s" && ./deploy-all.sh   # auto-detects worker role → joins + imports images
```

If passwordless SSH is ever configured, node 0's `deploy-all.sh` joins and seeds the
workers automatically — then it is a literal single command.

---

## 4. Deploy — step by step (what `deploy-all.sh` runs)

Run from inside `k8s/` on the indicated node. `export KUBECONFIG=/etc/rancher/k3s/k3s.yaml`
on node 0 (or use `k3s kubectl`).

| # | Command | Node | Result |
|---|---------|------|--------|
| 1 | `./01-install-server.sh` | node 0 | k3s control-plane up; prints the join token/URL |
| 2 | `K3S_URL=https://10.10.1.2:6443 K3S_TOKEN='<token>' ./02-install-agent.sh` | each worker | node joins (`kubectl get nodes` → Ready) |
| 3 | `./03-install-knative.sh` | node 0 | Knative Serving + Kourier + **Eventing** + default broker (**RMMap**) |
| 4 | `./06-strimzi.sh` | node 0 | Strimzi operator + Kafka `kafka-cluster` + 10 topics (**StateFun**) |
| 5 | `./05-redis.sh` | node 0 | in-cluster Redis (testing; real runs → dedicated box) |
| 6 | `./07-load-images.sh export` then `import` | node 0 then each node | StateFun images into every node's containerd |
| 7 | `./04-verify.sh` | node 0 | nodes Ready · cross-node pod networking · Knative hello-world |

---

## 5. Deploy the framework apps onto the env

The per-stage **function code already exists** (intra-node, single-box) under
`../Intra-Node Application_Benchmark/<W>/baseline/{cloudburst,rmmap}/` and the
streaming benchmark for StateFun. Only the **deployment layer** is new; reuse the
logic and repoint state at `REDIS_HOST`/Kafka.

### Cloudburst (→ `k8s/cloudburst/`)
The baseline uses the real `RedisCloudburst` runner from `compare_system/cloudburst`
(see `../Intra-Node Application_Benchmark/<W>/baseline/cloudburst/run_redis.py`).
Containerize that runner (its only external dep is Redis) and deploy it as a k8s
**Job** per workload, `REDIS_HOST`→the dedicated box. No scheduler/Anna stack needed
— the runner drives the Cloudburst data path over Redis.

### RMMap (→ `k8s/knative/`)
The baseline is already a **Flask app built for Knative** — `app/functions.py` posts
CloudEvents to `broker-ingress.knative-eventing…` (the broker `03-install-knative.sh`
now creates). Build its image from
`../Intra-Node Application_Benchmark/<W>/baseline/rmmap/Dockerfile`, distribute it
(`07-load-images.sh`), and deploy one Knative `Service` per stage + `Trigger`s wiring
the flow. State via Redis **ES protocol only — no MITOSIS kernel module** without
explicit authorization.

### Flink StateFun (→ `k8s/statefun/`)
1. Kafka env is created by step 4 (`06-strimzi.sh`).
2. Build the 3 `streaming-example-*` images, distribute with `07-load-images.sh`.
3. Set `imagePullPolicy: IfNotPresent`, then apply the master + 2 function
   deployments from
   `../Streaming Application_Benchmark/intra-node/baseline/FlinkStateFun/deployment/k8s/`
   (`flink-statefun.yaml`, `mediareview-function.yaml`, `socialnetwork-function.yaml`)
   — **skip** their `kafka.yaml`/`topics.yaml` (ours supersede them).
4. Scale `statefun-worker` replicas + `parallelism.default` across nodes.
5. Drive with `stream_bench.py batch` from a producer pod.
   Bootstrap: `kafka-cluster-kafka-bootstrap.default.svc:9092`.

**Measurement boundary** stays data-path only (compute + state transfer); the
k8s/Knative/Kafka control-plane overhead is excluded — same window as intra-node
(EXPERIMENT_PLAN §5.9).

---

## 6. Verify

```bash
cd "Tests/Inter-Node deployment/k8s" && ./04-verify.sh    # → "CLUSTER READY ✓"
```
Checks: all nodes `Ready`, cross-node pod-to-pod ping (CNI), and a Knative Service
that scales from zero and answers over Kourier. For StateFun also confirm:
`kubectl -n default get kafka,kafkatopic` → `kafka-cluster` Ready, 10 topics.

---

## 7. Scale to 9 nodes

Add the 5 new IPs to `AGENT_IPS` in `k8s/cluster.env`, run `./deploy-all.sh` (or
`02-install-agent.sh` + `07-load-images.sh import`) on each new box. No
control-plane change. Re-run `04-verify.sh`. Bump StateFun replicas/parallelism.

---

## 8. Troubleshooting

- **Worker won't join / cross-node ping fails** — open 6443/tcp, 8472/udp,
  10250/tcp; set `CLUSTER_IFACE` if the default route isn't the 10.10.1.x NIC.
- **`ksvc` not Ready / ImagePull errors** — nodes must reach `gcr.io/knative-*`;
  StateFun pods need the local images (`07-load-images.sh import` on that node +
  `imagePullPolicy: IfNotPresent`).
- **Kafka not Ready** — `kubectl -n default get kafka,pods`; 2 brokers + ZK on
  ephemeral storage take a few minutes; check the Strimzi operator log.
- **Kourier external IP `<pending>`** — `kubectl -n kourier-system get svc kourier`;
  k3s servicelb needs a free node IP, or use `kubectl port-forward`.
- **Strimzi rejects the manifest** — the streaming benchmark's original
  `kafka.yaml` is `v1beta1`/Kafka 2.4 (unsupported by current Strimzi); use
  `k8s/statefun/statefun-kafka.yaml` (v1beta2) shipped here instead.
- **Air-gapped** — pre-stage the k3s airgap tarball + binary; `kubectl apply`
  locally-downloaded `serving-*.yaml`, `kourier.yaml`, and the Strimzi install
  YAML; mirror Knative/Strimzi/sample images into a local registry. Scripts
  otherwise unchanged.

---

## 9. Teardown

```bash
# workers:        /usr/local/bin/k3s-agent-uninstall.sh
# control-plane:  /usr/local/bin/k3s-uninstall.sh        # removes Knative/Strimzi/Redis with the cluster
```

---

## 10. File map (`k8s/`)

```
k8s/
├── deploy-all.sh              # one-command, role-aware orchestrator
├── cluster.env               # topology + versions + REDIS_HOST + image list
├── 01-install-server.sh      # k3s control-plane (node 0)
├── 02-install-agent.sh       # k3s join (each worker)
├── 03-install-knative.sh     # Knative Serving + Kourier        (RMMap)
├── 06-strimzi.sh             # Strimzi + Kafka + topics          (Flink StateFun)
├── 07-load-images.sh         # StateFun image export/import to every node
├── 05-redis.sh               # in-cluster Redis                  (Cloudburst/RMMap KV)
├── 04-verify.sh              # smoke test
├── statefun/statefun-kafka.yaml   # Kafka cluster + 10 topics (Strimzi v1beta2)
├── cloudburst/  knative/  statefun/   # per-framework app manifests (READMEs)
└── images/                   # StateFun image tarballs (gitignored)
```
