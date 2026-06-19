# RTSFaaS multi-node test (inter-node, track E)

Distributes RTSFaaS across the cluster over RoCE — `database`+`driver`+`client` on
node0, one `worker` per worker node — using the in-memory store (no TiKV yet) and
`isRDMA=1`. Reuses the built `rtfaas:1.0` image + the single-node fixes
(`../../Streaming Application_Benchmark/intra-node/baseline/RTSFaaS/single_node/`).
Driver: `run-rtsfaas-cluster.sh`.

## Cluster facts (this cluster)

RoCE IP → reverse-DNS name (RTSFaaS matches workers by `getHostName()`, so these
exact names go in `workerHosts` — the script handles it):

| IP | name | role in this test |
|----|------|-------------------|
| 10.10.1.2 | node-0-link-0 | coordinator: database + driver + client |
| 10.10.1.1 | node-1-link-0 | worker 0 (default) |
| 10.10.1.4 | node-2-link-0 | worker 1 |
| 10.10.1.3 | node-3-link-0 | worker 2 |

(Note: .3/.4 map to node-3/node-2 — swapped. The script's `NAME[]` table has it right.)

## Self-install (the script builds everything)

The repo is git-synced across nodes, but git carries only **source** — not the
built jar, the docker image, or system packages. So `setup-node.sh` installs +
builds everything **per node** (idempotent: a no-op where already done):
RDMA `rdma_ucm`, Java 8 + Maven, the DB-role JVM-stays-alive patch, the
`rtfaas-jar-with-dependencies.jar`, and the `rtfaas:1.0` image. The runner triggers
it on every node with `SETUP=1` (or run `setup-node.sh` manually on each node).

## Two ways to run

**No-ssh, per-node (recommended if ssh between nodes isn't available).** You launch
one role per node by hand (e.g. in a tmux pane / terminal on each). All nodes read
the git-synced `cluster.env` and derive an identical RTSFaaS config, so the roles
find each other over RDMA — no coordinator/ssh needed. → see "Run (no ssh)" below.

**Automated (if passwordless ssh node0→workers works).** `run-rtsfaas-cluster.sh`
ssh's to install + launch + clean up the workers for you. → see "Run (ssh)" below.

Only prereq for the no-ssh path: **`sudo` works on each node** (for `apt-get` /
`docker` / `modprobe`), and the repo is git-synced to the same paths
(`/opt/myapp/WebAsShared`, `RTFAAS_DIR=/opt/myapp/compare_system/RTSFaaS`).
Everything else (image, jar, Java/Maven, kernel module, source patch) is built by
`setup-node.sh`.

## Run (no ssh) — this node coordinates + collects, others are workers

This is the "collect results on this node, other nodes just work as workers" flow.

1. **Edit `cluster.env`** once (git-synced to all nodes): `COORD_IP` = this node,
   `WORKER_IPS` = the other nodes, `APP`.
2. **On every node** (once): `bash setup-node.sh` — installs deps + builds the image
   (slow first time, ~minutes; cached after).
3. **On THIS node (the coordinator):**
   ```
   ./run-coordinator.sh            # starts database + driver here, waits for workers, then runs client
   ```
   It prints the worker commands to run, waits for them to connect, then runs the
   client and prints `Throughput: <X> records/sec` + the driver stats — collected here.
4. **On each worker node** (after the coordinator says "NOW start the workers"),
   workerId `0,1,2…` in `WORKER_IPS` order:
   ```
   ./run-role.sh worker 0          # on WORKER_IPS[0];  worker 1 on the next, etc.
   ```
   Leave each worker running (Ctrl-C to stop after the run finishes).

Scale by adding IPs to `WORKER_IPS` and starting `./run-role.sh worker <i>` on each.

## Run (ssh) — automated, if node0→workers ssh works

```
cd "Tests/Inter-Node deployment/rtsfaas"
SETUP=1 ./run-rtsfaas-cluster.sh MediaReview              # installs+builds+runs (1 worker)
WORKERS="10.10.1.1 10.10.1.4 10.10.1.3" ./run-rtsfaas-cluster.sh SocialNetwork
```
Needs passwordless ssh (`ssh-copy-id`) node0→workers + sudo on each.

Workload size/rate knobs are in the single-node `Env/{System.env,<App>.env}`
(`number`, `qps`) — the script copies them; edit there to scale.

## What it does (and what to expect)

- Writes a multi-node `Cluster.env` (driverHost/databaseHost = `node-0-link-0`,
  `workerHosts` = the workers' reverse-DNS names, `workerNum` = count) and a
  `Database.env` (in-memory store).
- Starts `database` then `driver` on node0 (detached), each `worker` on its node
  via ssh (detached), then `client` on node0 (foreground).
- The **client prints the result**: `Throughput: <X> records/sec` + latency — that
  is the inter-node number. (Unlike WasMem, RTSFaaS reports throughput from the
  client directly; no merge-node gate file.)

## First-test scope & next steps

- In-memory store (no TiKV) keeps the first run simple; the cross-node path is
  driver↔workers + workers↔database over **real-machine RDMA** (vs the single-node
  RoCE loopback). To reach RTSFaaS's full design point, switch `Database.env` to
  `isTiKV=1` and stand up a TiKV cluster (TiUP) — track E step 2 in the parent plan.
- Start with **1 worker** to validate the multi-machine RDMA handshake, then scale
  `WORKERS` to 2–3.

## Troubleshooting

- `ssh ... Permission denied (publickey)` → prereq #1 not done.
- worker container exits immediately → check the image is loaded on that node
  (prereq #2) and `rdma_ucm` is loaded (#3); inspect with `KEEP=1` then
  `ssh <ip> sudo docker logs rt-worker`.
- `rdma_create_event_channel ... No such file or directory` → `modprobe rdma_ucm`
  on that node.
- worker never registers / driver hangs → `workerHosts` name must equal the
  worker's `getHostName()` (reverse-DNS), not the IP (handled by the script's `NAME[]`).
