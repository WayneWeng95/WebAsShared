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

## Known issues — multi-node limitations (measured 2026-06-23, 4 nodes, in-mem store)

Both workloads run fine **single-node** (RTSFaaS: MediaReview 5,397 / SocialNetwork
6,155 req/s, `../../../intra-node/`); the problems are specific to the 4-node setup.

### 1. MediaReview HANGS multi-node (does not complete)
- **Symptom:** driver connects, all 8 executors per worker reach "ready", then **0
  finish** — workers GC-spin (escalating allocation, no exception), the client never
  prints `Throughput`. Single-node the *same* `Env/MediaReview.env` finishes in ~16 s.
- **Cause:** MediaReview's one event spans **3 tables** and is **100 % multi-partition**
  (`ratioOfMultiPartitionTransactionsForEvents=100`). With 1 worker all partitions are
  local; with 4 workers they shard across nodes, so every transaction needs cross-node
  commit/validation over RDMA, and MorphStream's distributed engine **livelocks**.
- **Ruled out (tested):** lowering contention `stateAccessSkewnessForEvents=0` → still
  hangs (>2 min, deadline). Setting `ratioOfMultiPartitionTransactionsForEvents=0` →
  still hangs (the 3-table access is structural). So it's the engine's cross-node
  multi-partition coordination, not contention or one knob.
- **TiKV would NOT fix it:** `TiKVStorageManager` uses `RawKVClient` (raw KV, no txns) —
  it only relocates state bytes; the coordination that livelocks is in the engine over
  RDMA. (Feasibility aside, `tiup` has no DNS here; Docker `pingcap/*` is the only route.)

### 2. SocialNetwork COMPLETES but does NOT scale
- **Symptom:** 4-node peak ≈ single-node peak (~6,000 req/s; sweep saturates at
  `number=500`→5,999 and **DID_NOT_COMPLETE** at ≥625 — see
  `results_rtsfaas_sweep_SocialNetwork.csv`). 4× the hardware ≈ 1× the throughput.
- **Cause:** end-to-end req/s is dominated by **fixed distribution overhead, not
  compute**. Per-executor engine rate is ~1.5–1.7 M events/s, but the all-to-all RDMA
  mesh setup ("Total Connection Time") grows with node count — **27 s @ 1 worker →
  64 s @ 4 workers** — while actual execution is only ~16 s. Plus a single shared
  in-mem store on node 0 and the cross-node shuffle cap aggregate throughput.
- **Above `number`≈500–625** the run overruns the RDMA buffers / deadline and never
  completes (matches the "100k overran single-node RDMA buffers" note).

### Recommendation / fallback
RTSFaaS's in-mem-store design does not scale out for these workloads (and MediaReview
is unrunnable multi-node). **Report multi-node RTSFaaS as a baseline limitation** and,
since horizontal scaling is unavailable, characterize RTSFaaS by **scaling executors
on a single node** instead (`Env/System.env` `threadNum`/`frontendNum`; nodes here have
16 logical CPUs vs the 8-core matched budget). See `../../../intra-node/`.

### Parallel-throughput ESTIMATE (`rt-parallel-estimate.py`)
Since true scale-out is unavailable, the embarrassingly-parallel **upper bound** is
the only meaningful "parallel" number: run the working single-node config
**independently on all 4 nodes concurrently** (each its own driver+database+worker+
client over RoCE loopback, no cross-node coordination) and sum the per-node
throughput. `rt-parallel-estimate.py <App>` does this via the agents (faasm 9600 to
stage a per-node Env — `driverHost`/`workerHosts`/`databaseHost` set to that node's
RoCE name, `workerNum=1`; rt-agent 9700 to launch the roles). No ssh, no Redis
(RTSFaaS can't use Redis — its store sits behind the RDMA layer that all backends
share, so a Redis/TiKV store changes neither the coordination livelock nor the mesh
overhead; it would only be slower).

Measured 2026-06-23, 4 nodes, in-mem store, `number=2000`:

| Workload | Real coordinated 4-node | Per-node (solo) | **Parallel estimate (Σ 4×)** | Efficiency |
|----------|-------------------------|-----------------|------------------------------|------------|
| SocialNetwork | 5,867 req/s (~1×, no scaling) | ~5,760 | **23,046 req/s** (~3.9×) | ~25% |
| MediaReview   | **hangs** (unrunnable)         | ~5,020 | **20,083 req/s** (~4×)   | n/a |

Interpretation: this is the *ceiling* (perfect partitioning, zero shared state). The
gap between it and the real coordinated SocialNetwork number (~4× → ~1×) is exactly
the cost of RTSFaaS's cross-node RDMA coordination; MediaReview never converges
coordinated, so the estimate is the only multi-node figure obtainable.
