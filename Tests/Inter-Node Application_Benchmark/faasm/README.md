# Faasm inter-node workload tests

Per-workload Faasm experiments for the inter-node application benchmark. The Faasm
**deployment substrate** (per-node agents + coordinator) lives separately in
`../../Inter-Node deployment/faasm/`; this folder holds the **workload tests** that
run on top of it and emit `results.csv`.

Faasm has no orchestration framework, so each stage is a fresh **wasmtime** Faaslet
(the AOT `wc.cwasm` module, reused from
`../../Intra-Node Application_Benchmark/WordCount/baseline/faasm/demo/`), and state
crosses nodes through the shared **Redis** (Faasm KV) — the serialized transfer
WasMem's RDMA page-chain moves zero-copy.

## WordCount (first workload)

| File | Role |
|------|------|
| `wc_faaslet.py` | per-node runner: bridges Redis ↔ the wc.cwasm Faaslet (map/reduce) |
| `run_wordcount.py` | node-0 driver: split→KV, launch map/reduce Faaslets via agents, time, gate, `results.csv` |
| `run.sh` | ground-truth (1 mapper) vs distributed (N mappers); asserts the gate |

**Flow:** the driver splits the corpus into N newline-aligned chunks → Redis,
launches one `map` Faaslet per chunk across the nodes (via each node's agent), plus
a `reduce` Faaslet on node 0 that waits for all partials and merges. The gate is
`total_occurrences` (fan-out-invariant — identical at every mapper count/placement).

## Prerequisites

1. **Agents up** on every node:
   ```bash
   cd "../../Inter-Node deployment/faasm" && ./deploy.sh start && ./deploy.sh status
   ```
2. **Redis reachable from every node** at `REDIS_HOST` (cluster.env). A default Redis
   is loopback-only (protected mode) — on node 0:
   ```bash
   redis-cli -h 127.0.0.1 config set protected-mode no
   redis-cli -h 127.0.0.1 config set bind "127.0.0.1 10.10.1.2"   # private NIC only
   redis-cli -h 127.0.0.1 config rewrite
   ```
3. Workers need `python3` + `redis` + `wasmtime` (the substrate's `setup-node.sh`
   installs these); `wc.cwasm` + `wc_faaslet.py` are git-synced to the same path.

## Run

```bash
./run.sh                                        # corpus_large (50MB), 8 mappers, 3 reps
./run.sh "$PWD/../../../TestData/corpus_xlarge.txt" 16 5
# or directly:
python3 run_wordcount.py --mappers 8 --nodes 4 --reps 5
```

`results.csv` columns: `size_mb,mappers,nodes_used,makespan_ms_median,occurrences,expect,success,reps`.

## Notes
- **Measurement boundary** = end-to-end job latency. `t0` is captured *before* the
  input is staged into Redis, so the timed window is `input upload (split/broadcast →
  Redis KV) + agent dispatch + Faaslet compute + Redis state I/O`, ending at last
  Faaslet completion. Only the local disk read of the corpus/trades fixture and the
  final result `get` are outside the window.
- **Cold Redis per rep**: each rep starts with `FLUSHDB` (untimed) so no warm KV state
  is carried across reps — every rep does a genuine upload + read. Use a **dedicated
  Redis box** for headline runs (fairness §8); `FLUSHDB` clears the current DB only,
  so don't point these drivers at a Redis shared with other live state.
- For headline runs use the **dedicated Redis box** off the compute path (fairness §8);
  node 0 is fine for bring-up.
- Next workloads (FINRA, ML-*, Matrix, TeraSort) follow the same shape: a per-node
  `*_faaslet.py` wrapper + a `run_*.py` driver reusing the intra Faasm stage code.
