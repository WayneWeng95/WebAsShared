# RMMap (ES/Redis) inter-node harness — WordCount

Track-C baseline for the inter-node app benchmark: **RMMap** run on **Knative** with
state routed through the shared cluster Redis. The MITOSIS/dmerge RDMA data plane needs
a kernel module we are not authorized to load, so this is RMMap's **ES (external-store)
protocol** — the byte-for-byte analogue of
`../../../port_files/RMMap_exp_wordcount_app/functions.py` (`splitter_es`/`mapper_es`/
`reducer_es`), with the inter-stage IR serialized through Redis.

This is **Option A**: the DAG is driven from node 0 (like the Cloudburst harness), but
the parallel **map** stage is a pool of **Knative Service** pods invoked over HTTP — the
track-C (Knative, scale-from-zero) analogue of Cloudburst's track-B static executor pool.
Picking A over a full Knative-Eventing Broker/Parallel flow keeps the measured quantity
**identical** to every other bar (state-transfer cost in the stage bodies) instead of
adding eventing-layer latency none of the other systems pay, while still exercising real
Knative Services, autoscaling, and genuine cold-start.

## Architecture

```
node 0 (driver.py)                          Redis (baselines ns, NodePort :30679)
  ├─ read corpus, shard into N              ┌───────────────────────────────┐
  │  newline-aligned chunks  ───SET────────▶│ <uid>_chunk_0 .. _chunk_{N-1} │
  │                                         └───────────────────────────────┘
  ├─ N concurrent HTTP POST ──▶ Kourier gw ──▶ wc-mapper ksvc (60 pods, 15/node)
  │   {chunk_key, result_key}   (Host hdr)       GET chunk → count → SET partial
  │                                              ↘ returns busy_ms (es+sd+exec)
  ├─ GET N partials ◀──────────────────────── <uid>_res_i
  └─ wc_reduce → makespan + total_job
```

node 0 is on the cluster subnet but **outside the pod mesh**, so it cannot use in-cluster
service DNS — it posts to the **Kourier external LB IP** with the ksvc route as the
`Host` header (Knative routes by Host).

## Files

| File | Role |
|------|------|
| `wc_ops.py`        | split / mapper / reducer bodies — **identical** to the Cloudburst harness (same `[a-z]+` tokenizer ⇒ `occ` is cross-checkable: 660,848,961 on the 4 GB corpus) |
| `mapper_app.py`    | the mapper stage as a Flask app (RMMap `mapper_es`: es-read → deserialize → count → serialize → es-write), returns per-invocation `busy_ms` |
| `Dockerfile`       | mapper image, tagged `dev.local/wc-mapper` so Knative **skips digest tag-resolution** (no registry; image is `ctr import`ed to every node) |
| `service-warm.yaml`| ksvc, `min-scale=60` — pre-warmed 60-pod fan (apples-to-apples with Cloudburst's 60 static pods) |
| `service-cold.yaml`| ksvc, `min-scale=0` + scale-to-zero — strict cold-start |
| `driver.py`        | node 0: shard+upload, concurrent HTTP fan-out, gather+reduce; emits the CSV |
| `run.sh`           | `./run.sh warm 15` / `./run.sh cold 5` — resolves gateway+Host, deploys, runs |

## Metrics (match `../../cloudburst` + `.../Inter-Node Application_Benchmark/{wasmem,faasm}`)

- **`makespan_mean_ms`** — end-to-end wall time of the whole KVS-routed DAG (bar base).
- **`total_job_mean_ms`** — `split`(node0) + Σ every mapper pod's `busy_ms`
  (es-read + deserialize + execute + serialize + es-write) + `reduce`(node0) — the full
  bar (Σ busy node-seconds).
- gate: **`occurrences`** = total word count, fan-out-invariant, equals the Cloudburst run.

CSV schema adds a **`variant`** column (`warm`/`cold`) vs the Cloudburst schema:
`size_mb,mappers,nodes_used,variant,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,occurrences,expect,success,reps`.
`plot_inter_bars.py` picks the largest size / most reps, so the 15-rep **warm** row is the
headline RMMap bar; **cold** is kept as a labeled secondary measurement.

## Warm vs cold (measure BOTH — EXPERIMENT_PLAN §C)

- **warm** (`min-scale=60`): the full fan is pre-warmed, no startup cost in the measured
  window — steady-state reuse, comparable to Cloudburst's static pool.
- **cold** (`min-scale=0`): `driver.py --ensure-zero` waits for the pool to drain to 0
  before each rep; the fan-out then pays real pod-scheduling + container + app-init for up
  to 60 pods (Knative's activator buffers the 60 requests until pods are Ready, so makespan
  captures the cold tax). Run fewer reps (cold reps are slow — each waits for scale-down).

## Deploy + run

```bash
# build + import the mapper image to every node's containerd (no registry):
sudo docker build -t dev.local/wc-mapper:latest .
sudo docker save dev.local/wc-mapper:latest -o wc-mapper.tar
sudo k3s ctr images import wc-mapper.tar                         # node 0
for n in node-1 node-2 node-3; do scp wc-mapper.tar $n:/tmp/ && \
  ssh $n "sudo k3s ctr images import /tmp/wc-mapper.tar"; done

./run.sh warm 15     # headline RMMap bar  (scales cloudburst-executor to 0 first)
./run.sh cold 5      # strict cold-start
```

> **Rebuild the image after editing `mapper_app.py`/`wc_ops.py`** — pods run the
> *imported* image, then `kubectl delete ksvc wc-mapper` + re-apply (Knative cuts a new
> revision only on spec change; a same-tag image change needs the revision recreated).

## Results so far (4 GB, fanout 60, 4 nodes)

| variant | makespan | total_job | occ | reps |
|---------|---------:|----------:|-----|-----:|
| **warm** | 41.1 s ± 2.7 | 1157.9 s | 660,848,961 | 15 |
| cold     | 44.0 s ± 1.4 |  919.3 s | 660,848,961 | 5 |

`occ` == the Cloudburst run (same corpus + tokenizer); RMMap pays the same
serialize-through-KVS cost as Cloudburst plus the Knative serving tax (per-pod
queue-proxy sidecar + Kourier hop), so its full bar (total_job) is the tallest. Cold's
makespan tops warm's by the 60-pod cold-start tax (~+3 s); its lower total_job reflects
fresh per-rep pods vs the warm pool's sustained CPU contention over the full 15-rep
window. For comparison: WasMem 13.8 s / 52.0 s · Faasm 15.7 s / 550 s · Cloudburst
37.1 s / 681.8 s (makespan / total_job).
