# RTSFaaS — inter-node (4-node cluster, in-memory RDMA store)

The cluster baseline for the inter-node streaming benchmark (vs WasMem). Lifts the
working single-node recipe (`../../../intra-node/baseline/RTSFaaS/single_node/`) to
4 nodes, keeping its **in-memory RDMA store** (`Env/Database.env isTiKV=0`, run the
`database` role) — **no TiKV** (not installed here; a durable store would break the
FT-off parity with WasMem's no-persist throughput line).

## What's dedicated to RTSFaaS here

Instead of RTSFaaS's shipped `scripts/FaaS/run-application.sh` (scp + ssh), we use a
**dedicated RTSFaaS agent/coordinator** — modeled on the faasm track-D agent but
RTSFaaS-specific (it knows the four roles and the privileged infiniband container):

- `rt-agent.py` — per-node HTTP agent; `POST /role {role,worker_id,app}` launches the
  same `docker run … rtfaas:1.0 <role>` the single-node baseline uses, on its node.
- `rt-coordinator.py` — places driver + database + client on node 0 and one `worker i`
  per node (single-node startup ordering + RDMA-handshake delays), polls the client to
  exit, scrapes throughput + latency, writes `results_rtsfaas_cluster.csv` (mean±std).
- `deploy.sh` — start/stop/status the rt-agents; `deploy.sh image` builds+distributes
  the image.
- `cluster.env` — orchestration topology. `Env/` — RTSFaaS config (Cluster.env = 4
  workers as RoCE reverse-DNS names in workerId order). `DockerFiles/` — role scripts
  (copied verbatim from single_node).

## Prerequisites

1. **`rtfaas:1.0` image on every node.** The image's jar is too large to
   git-distribute and is gitignored, so **build it on each node** from the
   git-synced source:  `./build-image.sh`  (re-applies the uncommitted fix #1
   patch, runs `mvn package` to regenerate the jars, then `docker build`). Needs
   `maven` + JDK 8 + docker on the node. (fixes #2–#7 are env/script-level and come
   from our mounted `Env/`+`DockerFiles/` at run time, not the image.) `deploy.sh
   image` (build-once + scp/load a tar) is the alternative if a node lacks the
   toolchain.
2. `sudo modprobe rdma_ucm` on every node (RDMA CM device).
3. Passwordless `sudo docker` (privileged containers) on every node, or edit
   `DOCKER=` in `cluster.env`.
4. Identical repo path on every node (`/opt/myapp/WebAsShared`).

## Run

```
./deploy.sh start                 # rt-agents up on all nodes
./deploy.sh status                # confirm /health
./rt-coordinator.py --both --reps 15      # both workloads -> results_rtsfaas_cluster.csv
./deploy.sh stop
```

## Topology note (this cluster)

IP→reverse-DNS is **non-sequential**: `.2→node-0`, `.1→node-1`, `.3→node-3`,
`.4→node-2`. `Env/Cluster.env workerHosts` and `cluster.env WORKER_IPS` are both in
workerId order and must stay aligned (driver matches a worker by
`workerHosts[i].equals(getHostName())`).
