# RMMap (RDMA) inter-node harness — Matrix (SUMMA)

The **faithful** RMMap baseline for the SUMMA block matrix-multiply C = A·B, same approach as
the other RDMA harnesses: serialization-free state transfer over RDMA remote memory via
**user-space one-sided RDMA READ** (`librdmacm`/`libibverbs`) — **no** MITOSIS kernel module
(see `../SESSION_PROGRESS.md` §6).

Matrix is the first **object-heavy** workload: the inter-stage state is f64 matrix panels/
blocks, not flat records. The dominant transfer is the **panel broadcast** (tile→blocks):
each of the R·C blocks reads an A_i row-panel (BR×N) + a B_j col-panel (N×BC), so ≈R·C·(panel
pair) bytes move (64×32 MB ≈ 2 GB at 4096²) — exactly where register-once / read-many RDMA
wins over Cloudburst's per-rep panel upload + per-block `GET`.

```
node 0 (driver.py)                              block workers (R·C=64, SSH fan-out)
  ├─ build ONE buffer = [A row-panels | B col-panels]
  │   (A row-panels contiguous as-is; B col-panels repacked contiguous)
  ├─ reg_mr ONCE via serve_idx (R+C bounds) ─▶  block (i,j): read chunk[i]=A_i, chunk[R+j]=B_j
  │   (publish: register-once)                  │ einsum('ik,kj->ij', optimize=False)  (naive, =bars)
  └─ sum partial checksums = gate            ◀─┘ RESULT partial=int(C_ij.sum())
```

`grid(W)` gives a balanced R×C (R≤C, R·C==W); W=64 → 8×8, BR=BC=512. The block kernel is the
**naive `np.einsum(optimize=False)`** — numpy's nested-loop contraction, **not** BLAS — so it
matches the Cloudburst (einsum) and Faasm/WasMem (WASM ikj) bars (fairness §5: the comparison
is the data substrate, not the kernel). The gate (Σ of all C entries) = **1,391,095,867,672**,
identical across **all four** systems (the checksum is kernel-order-stable here — verified).

**Why partial checksums, not shipped C blocks:** each block reports `int(C_ij.sum())` and the
driver sums them. Shipping the 2 MB C blocks to a reducer (64×2 MB ≈ 128 MB) is small vs the
panel broadcast (64×32 MB ≈ 2 GB) and identical across systems, so — like the WordCount RDMA
mappers reporting counts — it is omitted as not the compared transfer.

The **panel buffer is published ONCE** (register-once / read-many — the DMERGE win the KVS bars
don't get for the broadcast); per rep the block workers only RDMA-read + einsum.

## Files
| File | Role |
|------|------|
| `rdma_ts` / `rdma_ts.c` | RDMA transport, **reused from `../rmmap-rdma-terasort/`** — `serve_idx` (serve a buffer by explicit byte bounds = the A/B panels) + `read` (one-sided `RDMA_READ` of chunk[idx]). |
| `matrix_worker.py` | block worker: `rdma_ts read` A_i + B_j → `einsum` → one `RESULT partial=` line. |
| `driver.py` | node 0: build+publish the panel buffer, batched SSH fan-out (64 blocks round-robin to nodes), sum partials, CSV `variant=rdma`. |

## Build + run
```bash
# rdma_ts reused from ../rmmap-rdma-terasort (serve_idx + read); rebuild if needed:
#   gcc -O2 -o rdma_ts rdma_ts.c -lrdmacm -libverbs
# (driver.py scps rdma_ts + matrix_worker.py to /tmp on node-1..3; nodes need numpy)
kubectl -n baselines scale deploy/cloudburst-executor --replicas=0       # free node CPU
python3 driver.py --a /opt/myapp/WebAsShared/TestData/matrix_a_4096.bin \
  --b /opt/myapp/WebAsShared/TestData/matrix_b_4096.bin --matrix-n 4096 --workers 64 \
  --reps 15 --server-ip 10.10.1.2 --serve-port 18620 --expect 1391095867672 \
  --csv "../../Inter-Node Application_Benchmark/rmmap/results_matrix.csv"
kubectl -n baselines scale deploy/cloudburst-executor --replicas=60       # restore
```

**Deployment caveat:** block workers run as **host processes over SSH** (how RDMA jobs run),
not pods — exposing the RDMA device into containers needs a device plugin. The transfer
mechanism under study (RDMA vs KVS) is unchanged; only the launcher differs from the ES bar.
