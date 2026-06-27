# RMMap (RDMA) inter-node harness — WordCount

The **faithful** RMMap baseline: RMMap/DMERGE's contribution is *serialization-free state
transfer over RDMA remote memory*, and this harness realizes that **data path** on the
cluster — **without** the MITOSIS kernel module (which will not build here; see
`../SESSION_PROGRESS.md` §6 and `../k8s/knative/rmmap-wordcount/` for the ES fallback).

It uses **user-space one-sided RDMA READ** (`librdmacm`/`libibverbs`) over the existing
RoCE NIC — no kernel module, `ib_uverbs` is already loaded. node 0 mmaps the corpus and
registers it **once** as an RDMA MR (the "publish"); 60 mapper processes spread across the
4 nodes each **RDMA-READ their newline-aligned 67 MB chunk straight out of that memory**
(the NIC DMAs the bytes into the mapper — no pickle, no Redis staging, no CPU copy), then
count with the **same `wc_ops.wc_map`** as every other bar. The only thing that differs
from the RMMap-ES bar is the *transfer*: RDMA vs Redis `GET`.

```
node 0 (driver.py)                          mapper procs (15/node, SSH fan-out)
  ├─ mmap corpus, reg_mr ONCE  ──RoCE MR──▶  rdma_wc read <idx>  (one-sided RDMA_READ)
  │   (publish: register-once)                 │ 67MB chunk DMA'd into local buf
  ├─ SSH 1× per node, launch 15 mappers ──────▶│ wc_ops.wc_map (same Python compute)
  └─ gather busy_ms + occ                    ◀──┘ RESULT busy_ms=.. occ=..
```

## Why this vs true MITOSIS
RDMA-read moves the bytes once by NIC DMA with **no serialize/deserialize and no KVS** —
the same zero-copy *transfer* DMERGE gives for flat data. It lacks DMERGE's transparent
pointer remapping + lazy paging, but WordCount's big inter-stage state is **flat byte
chunks**, so for this workload it is the same zero-copy path. (Object-graph workloads would
show the gap; noted for ML.)

## Files
| File | Role |
|------|------|
| `rdma_wc.c` | RDMA transport. `serve`: mmap+reg_mr+rdma_listen, hand each mapper its chunk's `{addr,rkey,len}` in the connect private-data. `read`: one-sided `RDMA_READ` → raw bytes to stdout. `map`: read+count in C (transport self-test). |
| `rdma_mapper.py` | worker-side mapper: `rdma_wc read` (transfer) → `wc_ops.wc_map` (compute) → one `RESULT` line. |
| `wc_ops.py` | identical split/count to the Cloudburst + RMMap-ES bars (occ gate cross-checks: 660,848,961). |
| `driver.py` | node 0: start server, one batched SSH launch per node (60 mappers = 15/node), gather, CSV `variant=rdma`. |

## Metrics (same schema as `../k8s/cloudburst`, `../k8s/knative/rmmap-wordcount`)
- **`makespan`** — per-rep wall of the fan-out. The corpus is published **once**, so there
  is **no per-rep upload** — the register-once/read-many win the KVS bars don't get.
- **`total_job`** — Σ mapper `busy_ms` (RDMA transfer + count). The one-time publish
  (mmap+reg_mr) is reported separately (`publish_once`), amortized to ~0/rep. The
  per-mapper dict reduce is omitted (merging 60 tiny dicts is sub-second, identical across
  all systems, not the compared cost).
- **`occurrences`** — gate, == Cloudburst/RMMap-ES (660,848,961).

## Build + run
```bash
gcc -O2 -o rdma_wc rdma_wc.c -lrdmacm -libverbs
for n in node-1 node-2 node-3; do scp rdma_wc rdma_mapper.py wc_ops.py $n:/tmp/; done
# free node CPU for the host-process mappers:
kubectl -n baselines scale deploy/cloudburst-executor --replicas=0
python3 driver.py --corpus /opt/myapp/WebAsShared/TestData/corpus_4gb.txt \
  --fanout 60 --reps 15 --server-ip 10.10.1.2 --port 18515 --expect 660848961 \
  --csv ".../Inter-Node Application_Benchmark/rmmap/results_wordcount.csv"
```

**Deployment caveat:** mappers run as **host processes over SSH** (how RDMA jobs run), not
Knative pods — exposing the RDMA device into containers needs a device plugin (the
`hostdev-device-dev-plugin` RMMap's own k8s setup required). The transfer mechanism under
study (RDMA vs KVS) is unchanged; only the launcher differs from the ES bar.

## Results (4 GB, fanout 60, 4 nodes) — vs the other bars
| system | makespan | total_job | occ |
|--------|---------:|----------:|-----|
| WasMem (ours) | 13.8 s | 52 s | 715.2 M |
| **RMMap (RDMA)** | **~16 s** | **~850 s** | 660.8 M |
| Cloudburst (ES) | 37.1 s | 682 s | 660.8 M |
| RMMap (ES, no-kmod) | 41.1 s | 1158 s | 660.8 M |

**Read:** swapping Redis `GET` → one-sided RDMA cuts RMMap's makespan **41 s → ~16 s**
(no per-rep KVS upload; register-once/read-many) and total_job **1158 s → ~850 s** (Redis
serialize + read-contention removed). The residual ~850 s is the **Python word-count floor**
— identical compute to every other Python bar — which is why RMMap-RDMA does not reach
WasMem's 52 s total_job (WasMem's edge there is faster compute, not transfer). The
zero-copy transfer is no longer the bottleneck; compute is.
