# Faasm inter-node deployment (track D)

Faasm is **clean-slate** — Faaslets (WASM/SFI) + Faabric, no orchestration framework
of its own (Knative is Faasm's *competitor*, not its substrate). So unlike Cloudburst
(k8s) and RMMap (Knative), Faasm gets a **tiny per-node agent** that launches Faaslets
on its node — the baseline-side analogue of WasMem's `node-agent`, and the per-node
placement the others get from k8s/Knative. State moves through the shared **Redis**
(Faasm KV). This is EXPERIMENT_PLAN.md §5 (track D).

In this repo the Faasm baseline runs each function stage as a fresh **wasmtime**
instance over an AOT `.cwasm` module, state via Redis — see
`../../Intra-Node Application_Benchmark/<W>/baseline/faasm/`. This folder distributes
that across the cluster.

## Files

| File | Run on | Purpose |
|------|--------|---------|
| `cluster.env`     | — | topology + `REDIS_HOST` + agent port (edit first) |
| `setup-node.sh`   | each node | install wasmtime + wasm target + python + redis client |
| `agent.py`        | each node | per-node Faaslet launcher (HTTP: `/launch /status /stop /stage /metrics`) |
| `deploy.sh`       | any node | start/stop/status the agents; `run` the coordinator |
| `coordinator.py`  | node 0 | place Faaslets per node, stage modules, time, write `results.csv` |
| `job-example.json`| — | example placement spec (WordCount-shaped) |

## Steps

```bash
cd "Tests/Inter-Node deployment/faasm" && vim cluster.env   # set REDIS_HOST etc.
chmod +x *.sh

# 1. Prereqs — once per node (SSH between nodes is publickey-blocked → run locally on each):
./setup-node.sh

# 2. Deploy the agents — on node 0; starts workers too if SSH works, else prints
#    the one command to run on each worker:
./deploy.sh start
./deploy.sh status        # ✓ every node's agent healthy

# 3. Run a job (after porting a workload's Faaslet spec — see below):
./deploy.sh run job-example.json --reps 5
#    → results.csv: workload,nodes_used,faaslets,makespan_ms,total_exec_ms,success,rep

# teardown:
./deploy.sh stop
```

## Porting a workload (the remaining per-workload work)

`job-example.json` shows the shape; to run a real workload, point `stage.src` at that
workload's AOT Faaslet module and wire each Faaslet's `cmd`/args to its real
protocol, reusing the stage logic in
`../../Intra-Node Application_Benchmark/<W>/baseline/faasm/`. Each Faaslet reads/writes
its state in the shared Redis (`REDIS_HOST`), so the cross-node data path is Redis —
the serialized transfer WasMem's RDMA page-chain moves zero-copy.

**Measurement boundary** = data-path only (Faaslet compute + Redis state transfer);
agent/launch overhead is excluded, matching the intra-node window (EXPERIMENT_PLAN §5.9).

## Notes
- The agent is **stdlib-only** Python (no Flask) and supervises Faaslet processes;
  it never touches state — every node talks to the same neutral Redis box.
- Use the **dedicated Redis box** for real runs (off the compute path, fairness §8);
  the k8s kit's `05-redis.sh` can provide one for a smoke test.
- Scale to 9 nodes: add IPs to `WORKER_IPS`, `setup-node.sh` + `deploy.sh start` on each.
