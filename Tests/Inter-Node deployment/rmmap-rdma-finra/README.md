# RMMap (RDMA) inter-node harness — FINRA

The **faithful** RMMap baseline for FINRA, same approach as the WordCount/TeraSort harnesses:
RMMap/DMERGE's contribution is *serialization-free state transfer over RDMA remote memory*,
realized with **user-space one-sided RDMA READ** (`librdmacm`/`libibverbs`) over the RoCE NIC
— **no** MITOSIS kernel module (see `../SESSION_PROGRESS.md` §6).

FINRA is a **MAP-only audit fan** (no shuffle): the inter-stage state is just a violation
**count** per worker (like WordCount), but the input is read **~8×** (each of the 8 rule-groups
scans the trades), so it is *input-read-heavy* — exactly where register-once / read-many RDMA
wins over the KVS upload + per-worker `GET`.

```
node 0 (driver.py)                              audit workers (SSH fan-out, ~58)
  ├─ mmap trades, reg_mr ONCE ──RoCE MR──▶  stateless (rule,s): read chunk[s] (1/S slice)
  │   as S newline-aligned chunks            stateful  (rule):   read ALL S chunks = full corpus
  │   (publish: register-once)               │ finra_rules.audit_rule (same Python compute)
  └─ one batched SSH per node, gather      ◀─┘ RESULT viol=..
```

The 5 **stateless** rules (0,1,5,6,7) are each sharded into S disjoint slices (per-shard
counts sum exactly); the 3 **stateful** rules (2,3,4 = wash/spoofing/concentration) each read
the whole corpus (their cross-(account,symbol) state can't come from a slice). Effective fan =
3 + 5·S; a requested F → S = round((F−3)/5) (F=60 → S=11 → 58 workers — matches Faasm/WasMem).

Only chunk 0 carries the CSV header, so a stateless worker skips its first line **only** for
slice 0 (`skip_header = idx==0`); a stateful worker reassembles all S chunks and skips the one
header. The parse + 8 rule bodies are the identical `finra_rules` every Python bar runs — so
the gate is **exactly** the wasm bars' too: **violations = 2,271,415** (FINRA's Python port
matches `finra.rs`; unlike WordCount/TeraSort there is no Python-vs-wasm gate split here).

## Files
| File | Role |
|------|------|
| `rdma_ts` / `rdma_ts.c` | RDMA transport, **reused from `../rmmap-rdma-terasort/`** — `serve` (S newline-aligned chunks) + `read` (one-sided `RDMA_READ` of chunk[idx] → raw bytes). |
| `finra_rules.py` | the frozen 8-rule audit spec (copy of the intra-node file + a `skip_header` flag for interior slices). |
| `finra_worker.py` | worker: `rdma_ts read` its trades (shard or full) → `parse_trades` + `audit_rule` → one `RESULT` line. |
| `driver.py` | node 0: trades server, batched SSH fan-out (≈58 workers, round-robin to nodes), sum counts, CSV `variant=rdma`. |

## Build + run
```bash
# rdma_ts is reused from ../rmmap-rdma-terasort (serve + read); rebuild if needed:
#   gcc -O2 -o rdma_ts rdma_ts.c -lrdmacm -libverbs
# (driver.py scps rdma_ts + finra_worker.py + finra_rules.py to /tmp on node-1..3 itself)
kubectl -n baselines scale deploy/cloudburst-executor --replicas=0     # free node CPU
python3 driver.py --trades /opt/myapp/WebAsShared/TestData/finra_5m.csv \
  --fanout 60 --reps 15 --server-ip 10.10.1.2 --port 18515 --expect 2271415 \
  --csv "../../Inter-Node Application_Benchmark/rmmap/results_finra.csv"
kubectl -n baselines scale deploy/cloudburst-executor --replicas=60     # restore
```

**Deployment caveat:** workers run as **host processes over SSH** (how RDMA jobs run), not
pods — exposing the RDMA device into containers needs a device plugin. The transfer mechanism
under study (RDMA vs KVS) is unchanged; only the launcher differs from the ES bar.
