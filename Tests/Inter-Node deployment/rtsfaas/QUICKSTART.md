# RTSFaaS cluster — quick start

Run RTSFaaS across the cluster: **this node = coordinator (collects the result),
other nodes = workers**. No ssh between nodes; they coordinate via the git-synced
`cluster.env` + RDMA. Full details in [`README.md`](README.md).

All commands are run from this directory on each node:
```
cd "/opt/myapp/WebAsShared/Tests/Inter-Node deployment/rtsfaas"
```

---

## 1. One-time, on EVERY node (coordinator + workers)
```
bash setup-node.sh
```
Installs `rdma_ucm` + Java/Maven, applies the DB-role patch, and builds the
`rtfaas:1.0` image from the git-synced source. Slow the first time (~minutes,
downloads maven deps + builds image); a no-op afterwards.

## 2. Set the topology ONCE — edit `cluster.env`, then git-sync to all nodes
```
COORD_IP=10.10.1.2          # THIS node (coordinator + result collector)
WORKER_IPS="10.10.1.1"      # worker node(s); e.g. "10.10.1.1 10.10.1.4 10.10.1.3"
APP=MediaReview             # or SocialNetwork
```
IP → node: 10.10.1.2=node0, 10.10.1.1=node1, 10.10.1.4=node2, 10.10.1.3=node3.

## 3. Start the run

### On THIS node (coordinator) — collects the result here
```
./run-coordinator.sh
```
Starts `database` + `driver` here, prints the worker commands, waits for the
workers to connect, then runs the `client` and prints:
```
======== RTSFaaS MediaReview — cluster (N worker(s)) ========
Throughput: <X> records/sec
Average Latency: <Y> ms
--- driver ---
... finished with throughput ... / average latency ...
```

### On EACH worker node — after the coordinator prints "NOW start the workers"
workerId `0,1,2,…` in `WORKER_IPS` order (worker 0 on the first IP, etc.):
```
./run-role.sh worker 0
```
Leave it running; Ctrl-C to stop after the run completes.

---

## Order & notes
- Start `run-coordinator.sh` **first** (the driver/database must be listening), then
  start the workers — RTSFaaS workers connect *to* the driver.
- Scale: add IPs to `WORKER_IPS`, run `./run-role.sh worker <i>` on each.
- Switch workload: set `APP=SocialNetwork` in `cluster.env`.
- Keep containers up to inspect: `KEEP=1 ./run-coordinator.sh`.
- This is the in-memory store (no TiKV). For RTSFaaS's full design point, switch
  `Database.env` to `isTiKV=1` + a TiUP/TiKV cluster (see the parent deployment plan).

## If it doesn't connect
- worker exits immediately → `bash setup-node.sh` on that node (image/module missing).
- `rdma_create_event_channel ... No such file or directory` → `sudo modprobe rdma_ucm` on that node.
- driver never sees a worker → the worker's reverse-DNS name must match `cluster.env`'s
  `WORKER_IPS` mapping (handled automatically; check `getent hosts <ip>`).
