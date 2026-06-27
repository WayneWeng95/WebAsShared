# Cloudburst inter-node harness — WordCount + TeraSort (and beyond)

Distributed, k8s-native runner for the **Cloudburst** baseline on the 4-node k3s
cluster. It is the inter-node analogue of the single-process Cloudburst port
(`Tests/Inter-Node deployment/port_files/cloudburst_benchmarks/`, a Redis-backed
stand-in for Cloudburst's zmq/Anna control plane): a **pool of executor pods**
spread across the nodes pulls function tasks off a Redis queue and runs the **real**
stage bodies, routing every inter-stage value through Redis (the KVS-serialized
state transfer the comparison is about).

## Architecture

```
node 0 (driver.py)                         Redis (baselines ns, NodePort :30679)
  ├─ read corpus, shard into N              ┌───────────────────────────────┐
  │  newline-aligned chunks  ───SET────────▶│ <uid>_chunk_0 .. _chunk_{N-1} │
  ├─ LPUSH N wc_mapper tasks ──────────────▶│ cb:tasks (work queue)         │
  │                                         └───────────────────────────────┘
  │   executor pods (Deployment, 15/node)      ▲ BRPOP task   │ SET result
  │     run wc_ops.wc_map on their chunk ───────┘             ▼
  │                                          <uid>_res_i  +  LPUSH elapsed → cb:done:<uid>
  ├─ BLPOP N done signals (collect per-pod busy times)
  └─ GET N partials → wc_reduce → makespan + total_job
```

Sharding the corpus into N chunks (4 GB / 60 ≈ 67 MB each, all **< 512 MB**)
sidesteps Redis's single-value limit entirely — no `proto-max-bulk-len` hack, no
oversized key. Each `SET` is issued individually (NOT pipelined): pipelining all N
would buffer the whole corpus in Redis's query buffer and trip
`client-query-buffer-limit` (1 GB).

## Files

| File | Role |
|------|------|
| `wc_ops.py`   | the real WordCount split / mapper / reducer bodies (byte-for-byte the port's logic), shared by driver + executor |
| `ts_ops.py`   | the real TeraSort split / partition / merge / collect bodies, shared by driver + executor |
| `executor.py` | worker loop: `BRPOP` task → run `wc_mapper` / `ts_partition` / `ts_merge` (KVS read + compute + KVS write) → `LPUSH` its busy time |
| `driver.py`   | WordCount node 0: shard+upload corpus, dispatch, gather, reduce; emits the wasmem CSV schema |
| `driver_terasort.py` | TeraSort node 0: split+upload, two waves (partition scatter → barrier → merge gather), aggregate gate; same wasmem/faasm CSV schema as `results_terasort.csv` |
| `finra_rules.py` | the 8-rule FINRA audit spec (copy of the intra-node file + a `skip_header` flag), shared by driver + executor |
| `driver_finra.py` | FINRA node 0: upload full trades + S header-prefixed shards, one wave of 58 `finra_rule` tasks (5 stateless×S + 3 stateful full), sum violation counts; `results_finra.csv` schema |
| `driver_matrix.py` | Matrix (SUMMA) node 0: tile A row-panels + B col-panels, 64 `mat_block` tasks (naive `einsum(optimize=False)`, NOT BLAS — fairness §5), assemble the f64 checksum; `results_matrix.csv` schema. Gate = 1,391,095,867,672 (uniform across all systems — integer entries → order-independent). |
| `sgd_core.py` / `infer_core.py` | the shared integer ML kernels (copies of the intra-node baselines), reused verbatim by the ML ops + drivers so the gates match the wasm bars |
| `driver_ml_training.py` | ML-training node 0: split CSV → upload shards, W `ml_train` tasks (`grad_sum`), aggregate → `apply_update` → weight_checksum (1232) + accuracy; `results_ml_training.csv` |
| `driver_ml_inference.py` | ML-inference node 0: upload model + shards, W `ml_infer` tasks (`evaluate`), aggregate → accuracy + prediction_checksum (18,633,154, regenerated model); `results_ml_inference.csv` |
| `Dockerfile`  | executor image (`python:3.11-slim` + redis + cloudpickle + **numpy** + wc_ops/ts_ops/finra_rules/sgd_core/infer_core/executor) |

All six inter-node workloads are now ops in `executor.py`: `wc_mapper`, `ts_partition`/`ts_merge`,
`finra_rule`, `mat_block`, `ml_train`, `ml_infer`.
| `executor-deployment.yaml` | the executor pool (replicas spread across nodes via topologySpread) |

### TeraSort (`driver_terasort.py`)
The all-to-all shuffle, two waves over the same pool: **partition** (N `ts_partition` tasks,
each range-partitions its chunk into N owner buckets → `<uid>_bucket_<i>_<j>`, the scatter) →
barrier → **merge** (N `ts_merge` tasks, each gathers its column, sorts → `<uid>_summary_<j>`,
the gather). Runs at **fanout 4** (= the N range owners, matching wasmem/faasm `workers=4`).
gate = `(records, keysum, sorted)` = **12,884,902 / 8,310,813,852 / 1**, self-consistent across
reps (differs from wasmem/faasm's wasm-`ts.cwasm` gate the same way WordCount's occ differs
between the Python and wasm bars — each system self-consistent).

> **Pod memory:** TeraSort at fanout 4 loads a **~322 MB chunk** per partition task and builds
> its record lists + joined bucket blobs (peak ~1.3 GB), so the executor `limits.memory` was
> raised **1Gi → 4Gi** (`executor-deployment.yaml`). At 1Gi the partition pods OOM-killed
> mid-task — the task vanished with no result and no done signal, and only `DONE_TIMEOUT` +
> re-dispatch recovered it (slowly). `DONE_TIMEOUT` is also larger in `driver_terasort.py`
> (300 s vs 60 s): a fanout-4 task legitimately moves ~1/4 of the dataset through the single
> Redis, far slower than WordCount's 67 MB chunks, so 60 s would wrongly re-dispatch a live task.

## Metrics (match `../../../Inter-Node Application_Benchmark/{wasmem,faasm}/results_wordcount.csv`)

- **`makespan_mean_ms`** — end-to-end wall time of the whole KVS-routed DAG (the bar
  *base* in `analysis/plot_inter_bars.py`).
- **`total_job_mean_ms`** — **Σ busy node/pod-seconds** = `split` (node 0) + Σ every
  mapper pod's busy time (KVS read + count + KVS write) + `reduce` (node 0). The
  *full bar* (total execution time across all pods) — the **per-pod execution time
  summed over all pods**, not the inner-script time only.
- gate: **`occurrences`** = total word count, fan-out-invariant (identical across reps).

CSV schema: `size_mb,mappers,nodes_used,makespan_mean_ms,makespan_std_ms,total_job_mean_ms,occurrences,expect,success,reps`.

## Deploy + run

```bash
# 1. shared Redis (already up via 05-redis.sh) sized for the data:
kubectl -n baselines exec deploy/redis -- redis-cli config set maxmemory 24gb
kubectl -n baselines exec deploy/redis -- redis-cli config set maxmemory-policy noeviction

# 2. build + import the executor image to EVERY node's containerd, then deploy:
sudo docker build -t cloudburst-executor:latest .
sudo docker save cloudburst-executor:latest -o cb-exec.tar
sudo k3s ctr images import cb-exec.tar                      # node 0
for n in node-1 node-2 node-3; do scp cb-exec.tar $n:$PWD/ && \
  ssh $n "sudo k3s ctr images import $PWD/cb-exec.tar"; done
kubectl apply -f executor-deployment.yaml                  # 60 pods = 15/node

# 3. drive from node 0 (Redis reached via NodePort):
python3 driver.py --corpus TestData/corpus_4gb.txt --fanout 60 --reps 15 \
  --redis-host 10.10.1.2 --redis-port 30679 \
  --csv "../../../Inter-Node Application_Benchmark/cloudburst/results_wordcount.csv"
```

> **IMPORTANT — rebuild the image after editing `executor.py`/`wc_ops.py`.** The
> pods run the *imported* image, not the working-tree file. (A stale image once made
> the executor `LPUSH` the task **index** instead of the elapsed time, so the driver
> summed 0+1+…+59 = 1770 "ms" as `total_job` — a ~40× undercount. Rebuild + re-import
> + `kubectl rollout restart deploy/cloudburst-executor` whenever the code changes.)

## Results so far (4 GB, fanout 60, 4 nodes)

WordCount 4 GB head-to-head — Cloudburst pays the serial KVS upload + per-mapper
chunk read (the broadcast cost wasmem's RDMA zero-copy avoids), so it is the slowest:

| system | makespan | total_job | occ |
|--------|---------:|----------:|-----|
| WasMem | 13.8 s | 52.0 s | 715.2 M |
| Faasm  | 15.7 s | 550 s   | 715.2 M |
| **Cloudburst** | **37.1 s ± 0.8** | **681.8 s** | 660.8 M |
| RMMap (warm) | 41.1 s ± 2.7 | 1157.9 s | 660.8 M |

(15 reps, fixed harness; gate exact every rep, zero re-dispatches.)

`occ` differs from wasmem/faasm (660.8 M vs 715.2 M) because the corpus was
**regenerated** from the recovered 12 KB seed (`git` object `c1479760`) tiled to
exactly 4000 MiB — same *size* as the originals (the lost originals were not
byte-identical to the regen). Cloudburst's gate is self-consistent across reps.

## Status / next

- [x] Harness built; executor image on all 4 nodes; 60-pod pool deploys + spreads 15/node.
- [x] Small (50 MB) + 4 GB runs execute end-to-end; gate consistent.
- [x] **Timing bug fixed** (executor now reports real per-pod busy time; image rebuilt).
- [x] **BLPOP hang fixed:** a lost in-flight task (pod died after `BRPOP`, before
      writing a result → no result, no done signal) used to hang the single blocking
      `BLPOP` forever. Done signal now `"idx:ms"`; the driver verifies completion by
      **result-key presence** and **re-dispatches** lost tasks (60 s no-progress
      timeout, ≤8 rounds). Re-run never stalled.
- [x] **Re-run DONE** (15 reps @ 4 GB): makespan 37.1 s ± 0.8, total_job 681.8 s, occ
      660,848,961 (gate exact every rep). CSV written; `inter_node_bars` regenerated.
- [ ] Extend `executor.py` with the other workloads' stage bodies (terasort etc.).
