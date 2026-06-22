# Snapshot & Spawn — k8s + Knative cluster nodes

Helper for taking a node snapshot from **node 0** and spawning worker nodes for
inter-node cluster testing. Companion to `k8s deployment.md`.

## Current state (already done on node 0 = `10.10.1.2` / `eno1d1`)

- **k3s control-plane** `v1.29.6+k3s2` installed & `Ready` (kubeconfig
  `/etc/rancher/k3s/k3s.yaml`; Traefik disabled; flannel pinned to `eno1d1`).
- **Knative Serving** `v1.14.1` + **Kourier** (LB `10.10.1.2`) + **Eventing**
  `v1.14.1` (in-memory channel, MT broker, `default-broker` Ready).
- Verified: CNI cross-node ping OK; Knative scale-from-zero answers over Kourier.
- NOT installed (on purpose): Strimzi/Kafka (`06`), Redis (`05`).

### Config fixes applied to `cluster.env` (commit so they sync to all nodes)

| Key | Value | Why |
|-----|-------|-----|
| `CLUSTER_IFACE` | `eno1d1` | default route is public `eno1`; pin flannel VXLAN to the RoCE cluster NIC |
| `KOURIER_VERSION` | `v1.14.0` | net-kourier never published `v1.14.1` (script default 404'd) |

```bash
cd /opt/myapp/WebAsShared
git add "Tests/Inter-Node deployment/k8s/cluster.env"
git commit -m "k8s: pin CLUSTER_IFACE=eno1d1 and KOURIER_VERSION=v1.14.0"
```

---

## ⚠️ Before snapshotting

`k3s.service` is **enabled at boot** and a snapshot captures node 0's full server
identity (TLS certs tied to `10.10.1.2`, the node-token, the datastore). A naive
clone therefore boots as a **duplicate control-plane** — wrong for workers. The
real benefit of the snapshot is the **cached container images** (fast joins), not
a running server.

Pick ONE approach below.

### Approach A — golden image for workers (recommended)

Disable k3s at boot so clones come up clean (binary + images present, no server),
then snapshot:

```bash
sudo systemctl disable k3s     # clones won't auto-start a server
sudo systemctl stop k3s        # optional: quiesce before imaging
# >>> take the node snapshot now <<<
sudo systemctl enable --now k3s   # restore node 0 as control-plane
```

On each **spawned worker**, join as an agent (token below):

```bash
cd "Tests/Inter-Node deployment/k8s"
K3S_URL=https://10.10.1.2:6443 K3S_TOKEN='<TOKEN>' ./02-install-agent.sh
```

### Approach B — clone already booted as a server

If a clone came up running k3s (you didn't disable it), reset it to a worker:

```bash
sudo /usr/local/bin/k3s-uninstall.sh           # wipe the cloned server identity
cd "Tests/Inter-Node deployment/k8s"
K3S_URL=https://10.10.1.2:6443 K3S_TOKEN='<TOKEN>' ./02-install-agent.sh
```

### Join token

```
K3S_URL=https://10.10.1.2:6443
K3S_TOKEN=K10c1b5a285b2ab0c69cb9297d4a7a47fae127ab85e14317828d6801c38b80ba51a::server:280d9de7eaf5b841edb1dc3f9258d9d9
```

Also retrievable on node 0: `sudo cat /var/lib/rancher/k3s/server/node-token`
(and cached in the gitignored `.cluster-join`). If you rebuild node 0, the token
changes — re-read it.

---

## Networking prereqs (between all nodes)

Open: **TCP 6443** (API), **UDP 8472** (flannel VXLAN), **TCP 10250** (kubelet).
All k8s traffic must ride the `10.10.1.x` network — `CLUSTER_IFACE=eno1d1` and
`--node-ip` handle that, but each spawned node must own its own `10.10.1.x` IP on
`eno1d1` (matches `AGENT_IPS` in `cluster.env`: `10.10.1.1 10.10.1.3 10.10.1.4`).

---

## Verify after workers join (run on node 0)

```bash
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
kubectl get nodes -o wide                       # all should be Ready
cd "Tests/Inter-Node deployment/k8s" && ./04-verify.sh   # → CLUSTER READY ✓
```

`04-verify.sh` currently reports "1/4 nodes Ready" — expected until workers join;
the CNI and Knative checks already pass.

---

## Notes

- Run k8s experiments **one at a time** vs WasMem (fairness §5.4) — a live
  control-plane + pods steal CPU/RAM from a concurrent WasMem run.
- Scaling past 4 nodes: add IPs to `AGENT_IPS`, snapshot-spawn, join with
  `02-install-agent.sh`. No control-plane change.
- Teardown: workers `/usr/local/bin/k3s-agent-uninstall.sh`; node 0
  `/usr/local/bin/k3s-uninstall.sh`.

---

## Node 0 note

**What's running on this box (`node-0`, `10.10.1.2`/`eno1d1`):**

- k3s control-plane `v1.29.6+k3s2` (containerd + flannel CNI + servicelb, Traefik
  disabled), node `Ready`, kubeconfig at `/etc/rancher/k3s/k3s.yaml`.
- Knative Serving `v1.14.1` + Kourier ingress (LB IP `10.10.1.2`) + Eventing
  `v1.14.1` with in-memory channel, MT broker, and `default-broker` (`READY=True`).
  All pods `Running`. Magic DNS via sslip.io works — a hello-world ksvc scaled from
  zero and answered over Kourier.

Deliberately stuck to **k8s + Knative only** — did not run Strimzi/Kafka or Redis
(those are `06`/`05`, for the StateFun/Cloudburst baselines).

**Two config fixes made** (both in the git-synced `cluster.env`, currently uncommitted):

1. `CLUSTER_IFACE=eno1d1` — this node's default route is the public NIC `eno1`, so
   flannel VXLAN would have bound to the wrong interface; pinned it to the RoCE
   cluster NIC.
2. `KOURIER_VERSION=v1.14.0` — net-kourier never published a `v1.14.1` (the script's
   default 404'd); `v1.14.0` is the correct 1.14-line release and pairs with
   Serving 1.14.1.

Commit `cluster.env` so spawned nodes get these fixes when the folder syncs.

### ⚠️ Before you snapshot — important

`k3s.service` is enabled at boot and the snapshot captures node 0's full server
identity (TLS certs tied to `10.10.1.2`, the node-token, and the datastore). So a
naive clone boots as a **duplicate control-plane**, which is almost certainly not
what you want for workers. Pick one:

- **Workers from this image (typical):** the ~7 cached container images are the real
  win (fast joins). On each spawned clone, reset the server identity and rejoin as an
  agent — `sudo /usr/local/bin/k3s-uninstall.sh` then run `02-install-agent.sh` with
  the join token below. (Alternatively, `sudo systemctl disable k3s` before
  snapshotting so clones boot clean, then re-enable on node 0.)
- **Keep node 0 as the only control-plane:** leave this box as-is; spawned workers
  just run `02-install-agent.sh`.

Join command for workers (also in the gitignored `.cluster-join`):

```bash
K3S_URL=https://10.10.1.2:6443 \
  K3S_TOKEN='K10c1b5a285b2ab0c69cb9297d4a7a47fae127ab85e14317828d6801c38b80ba51a::server:280d9de7eaf5b841edb1dc3f9258d9d9' \
  ./02-install-agent.sh
```

After workers join, re-run `./04-verify.sh` — it'll go green once all 4 nodes are
`Ready` (the only failing check right now is the node count, expected pre-spawn).
Make sure ports 6443/tcp, 8472/udp, 10250/tcp are open between nodes.