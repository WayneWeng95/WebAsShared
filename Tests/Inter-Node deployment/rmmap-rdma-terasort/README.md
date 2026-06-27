# RMMap (RDMA) inter-node harness — TeraSort

The **faithful** RMMap baseline for TeraSort, the same approach as the WordCount harness
(`../rmmap-rdma-wordcount/`): RMMap/DMERGE's contribution is *serialization-free state
transfer over RDMA remote memory*, realized here on the cluster with **user-space one-sided
RDMA READ** (`librdmacm`/`libibverbs`) over the RoCE NIC — **no** MITOSIS kernel module
(it won't build here; see `../SESSION_PROGRESS.md` §6).

TeraSort is an **all-to-all shuffle** (the maximal-data-movement workload: the *entire*
dataset crosses the shuffle once), so unlike WordCount — where only the input read was
RDMA — here **both** the input read **and** the shuffle are one-sided RDMA READ:

```
node 0 (driver.py)                              partitioners (1/node, SSH fan-out)
  ├─ mmap records, reg_mr ONCE ──RoCE MR──▶  rdma_ts read <i>   (input read, zero-copy)
  │   (input publish: register-once)            │ chunk DMA'd in → ts_partition into N buckets
  │                                             │ write bucket file → reg_mr (publish buckets)
  │                                             └─ PART_READY ip=.. port=..
  │  (barrier: all N partitioners published)
  └─ launch N mergers ─────────────────────▶  mergers (1/node, SSH fan-out)
                                                │ rdma_ts read bucket[j] from EACH partitioner
                                                │   (shuffle GATHER, zero-copy, no KVS)
                                                │ ts_merge = sort the key range
                                                └─ RESULT records=.. keysum=.. sorted=..
```

The only thing that differs from the Cloudburst/Faasm bars is the **transfer**: the shuffle
buckets move by NIC DMA out of the producer's registered memory instead of Redis `SET`/`GET`.
The partition + sort **compute** is the identical `ts_ops` body every Python bar runs.

## Why this is the faithful shuffle
Each partitioner **publishes** its buckets by registering them as ONE remote-read MR
(`rdma_ts serve_idx`), and every merger **RDMA-READs** its owner column straight out of that
memory — no serialize, no KVS staging, no CPU copy on the producer. That is exactly DMERGE's
flat-state data path. (vs true MITOSIS it still lacks pointer remapping + lazy paging, but
TeraSort records are flat bytes, so for this workload it is the same zero-copy transfer.)

The **input corpus** is published **once** (register-once / read-many, the DMERGE win the KVS
bars don't get for the input read). The **shuffle buckets** are re-published **each rep**
(the partition wave re-runs), matching the KVS bars which re-scatter buckets to Redis each rep.

## Files
| File | Role |
|------|------|
| `rdma_ts.c` | RDMA transport. `serve`: input corpus, newline-aligned chunk by index. `serve_idx`: a partitioner's bucket file, served by EXPLICIT bucket boundaries (the shuffle publish). `read`: one-sided `RDMA_READ` of chunk[idx] → raw bytes (used for both the input read and the shuffle gather). |
| `ts_ops.py` | identical split / partition / merge bodies to the Cloudburst + Faasm bars (gate cross-checks). |
| `partitioner.py` | worker: `rdma_ts read` input chunk → `ts_partition` → write+`serve_idx` buckets → `PART_READY`. |
| `merger.py` | worker: `rdma_ts read` bucket[j] from each partitioner → `ts_merge` (sort) → one `RESULT` line. |
| `driver.py` | node 0: input server, two SSH-fanned waves (partition then merge), aggregate gate, tear down per-rep bucket servers, CSV `variant=rdma`. |

## Metrics (same schema as `../../Inter-Node Application_Benchmark/{wasmem,faasm}/results_terasort.csv` + a `variant` col)
- **`makespan`** — per-rep wall of partition→merge. The input corpus is published **once**
  (no per-rep input upload).
- **`total_job`** — Σ partition busy (RDMA read + partition) + Σ merge busy (RDMA gather + sort).
  The one-time input publish (mmap+reg_mr) is reported separately (`publish_once`).
- gate **`(records, keysum, sorted)`** — fan-out-invariant, self-consistent across reps and
  **== the Cloudburst run** (both run the Python `ts_ops` body): `12884902, 8310813852, 1`.
  (Differs from Faasm/WasMem's wasm-`ts.cwasm` gate the same way WordCount's occ differs
  between the Python and wasm bars — each system self-consistent.)

## Build + run
```bash
gcc -O2 -o rdma_ts rdma_ts.c -lrdmacm -libverbs
# (driver.py scps rdma_ts + the .py workers to /tmp on node-1..3 itself)
# free node CPU for the host-process workers:
kubectl -n baselines scale deploy/cloudburst-executor --replicas=0
python3 driver.py --records /opt/myapp/WebAsShared/TestData/terasort_1.2gb.txt \
  --fanout 4 --reps 15 --server-ip 10.10.1.2 --input-port 18515 \
  --expect 12884902,8310813852,1 \
  --csv "../../Inter-Node Application_Benchmark/rmmap/results_terasort.csv"
kubectl -n baselines scale deploy/cloudburst-executor --replicas=60   # restore
```

**Deployment caveat:** partitioners/mergers run as **host processes over SSH** (how RDMA jobs
run), not pods — exposing the RDMA device into containers needs a device plugin. The transfer
mechanism under study (RDMA vs KVS) is unchanged; only the launcher differs from the ES bar.
