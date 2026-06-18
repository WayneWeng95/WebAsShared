# TeraSort — WebAsShared vs RMMap / Faasm / Cloudburst  *(planned — workload #7)*

**Status: PLANNED / not built (added 2026-06-18).** New data-parallel workload that
adds the one structure the current five lack: an **all-to-all shuffle**. Runs in
**both** suites — intra-node (this folder, SHM page-chain) and inter-node
(`../../Inter-Node Application_Benchmark/`, RDMA page-chain).

> **Why TeraSort (vs TF-IDF, and vs PageRank the backup).** TF-IDF was rejected as
> too close to WordCount (same fan-out→gather shape). TeraSort is **distinct**: the
> entire input is **repartitioned across workers** in a shuffle, so it is purely
> *data-movement-bound* and stresses the **many-to-many** cross-node path that best
> showcases our zero-copy page-chain vs serialized network-KV. **PageRank is the
> documented backup** (iterative graph scatter-gather) — heavier (new graph kernel,
> convergence, irregular comms) and overlaps ML-training's iterative dimension, so
> it's deferred unless we specifically want the graph/placement-sensitivity story.
> See `../../Inter-Node Application_Benchmark/EXPERIMENT_PLAN.md` §8/§11.

```
partition (1)  ──map/range──►  sorter × N  ──shuffle (all-to-all)──►  merger × N  ──►  output
read records,                 sample keys,                           each range-owner
assign key ranges             route record → owner                   sorts its range
```

The interesting bytes are the **records crossing the shuffle** mapper→range-owner.
Unlike WordCount (per-mapper *partial maps* gathered 1-per-peer), here the **whole
dataset moves once** across an N×N exchange — the maximal data-movement workload.
On our system these move zero-copy (SHM intra / RDMA inter); RMMap-`ES`,
Faasm-state, and Cloudburst-Redis serialize every record through the KV — that delta,
multiplied by the *entire* input, is the result.

**Standard TeraSort data:** 100-byte records = 10-byte key + 90-byte payload
(Gensort convention). Sizes mirror the WordCount axis: ~50 MB / 500 MB / 1 GB
(→ ~0.5M / 5M / 10M records). A `gen_records.py` will emit deterministic random keys
so the sorted output is reproducible across systems.

---

## WebAsShared side  *(NEW guest — additive, no edits to existing stages)*

No sort guest exists yet. Add `Executor/guest/src/workloads/terasort.rs` with the
map → shuffle → merge stages, reusing the **existing `Shuffle` routing primitive**
(`Executor/host/src/routing/shuffle.rs` + the `Shuffle` node kind already used in
`DAGs/.../img_pipeline_auto_placement.json`). Planned stages:

- `ts_sample(slot)` — scan a sample, pick N−1 **range splitters** (balanced key
  boundaries); emits the partition map.
- `ts_partition(slot)` — route each record to its range-owner's sub-slot by key
  (zero-copy relink where contiguous, like `wc_distribute`); this feeds the Shuffle.
- `ts_merge(slot)` — each range-owner **sorts its received range** locally; output
  is globally ordered when ranges are concatenated in order.

DAGs to author (mirroring WordCount's set):
- `DAGs/.../terasort_demo.json` — single-node smoke DAG (`node-agent run`).
- `DAGs/symbolic_dag/terasort_auto_placement.json` — `placement:"all"` + `fanout:N`
  for the cluster-submit path (needed by the inter-node sweep + `gen_variants`).

This folder's `gen_dag.py` + `run.sh` sweep `(input size × N)` and write `results.csv`
in the shared shape (`size_mb,workers,topo,compute_ms,throughput_mb_s,records_per_s,peak_mem_mb,reps,checksum`).

**Correctness gate (trivial + bulletproof):** the output is (a) **globally sorted**
and (b) a **permutation of the input** — verify by a sortedness scan + a key
multiset **checksum** (Σ/XOR of all keys) that is **fan-out-invariant** (identical at
every N for a given input). Any mismatch = the shuffle dropped/duplicated a record.

---

## Baselines — none ship TeraSort natively → port all three (WordCount-shaped)

None of RMMap / Faasm / Cloudburst includes a sort workload, but the map→shuffle→reduce
plumbing is the **same shape as the WordCount ports** — clone each baseline tree and
swap the kernel (count → range-partition + local sort), keeping the Redis state path,
the per-N process model, and the CSV columns.

| Baseline | Reuse from | TeraSort change |
|----------|------------|-----------------|
| **RMMap (ES)** | `../WordCount/baseline/rmmap/` | splitter samples + range-partitions; each "mapper" writes its records to per-range Redis keys; range-owners `redis_get` their partition and sort. Profile keeps `sd_ms` (pickle) + `es_ms` (Redis) — both now scale with the *full dataset* (vs WordCount's smaller partial maps). **No MITOSIS module.** |
| **Faasm-like** | `../WordCount/baseline/faasm/demo/` | `terasort.rs` → `wasm32-wasip1` Faaslet: partition stage routes records to `range_<j>` Redis keys; merge Faaslet sorts a range. Same fresh-`wasmtime`-per-stage model. |
| **Cloudburst** | `../WordCount/baseline/cloudburst/` | Python DAG `ts_split → ts_partition_{0..N} → ts_merge_{0..N}` over the Redis-backed runner; records cross the KVS serialized. |

Common: same Gensort input + sizes as ours; ≥5 reps; the **multiset checksum gate**
must match ours at every N. Redis put/get **bytes** = the serialized shuffle volume
(≈ 1× input, the headline contrast vs our 0).

---

## Metrics & sweeps

- **Sweeps:** input size {50 MB, 500 MB, 1 GB} × parallelism N {1, 2, 4, 8, 16};
  topology {intra-node SHM (this folder), inter-node RDMA (sibling suite)}.
- **Report:** throughput (MB/s, records/s), e2e runtime, peak/billable memory,
  RDMA/shuffle bytes, **serialization cost** (ours = 0; baselines = full-dataset
  ser/deser through the KV). Median + spread.
- **Headline:** TeraSort moves the *entire* dataset across the exchange, so the
  zero-copy-vs-serialized gap is the largest and most legible of the suite, and grows
  monotonically with input size.

---

## Step-by-step  *(all pending — new workload)*

- [ ] `gen_records.py` — deterministic 100-byte Gensort records (key + payload); sizes 50/500/1000 MB.
- [ ] Guest `terasort.rs` (`ts_sample` / `ts_partition` / `ts_merge`) over the existing `Shuffle` primitive — additive, no edits to existing stages.
- [ ] DAGs: `terasort_demo.json` (smoke) + `terasort_auto_placement.json` (cluster/sweep).
- [ ] Validate our side: `node-agent run` smoke → sorted + checksum gate holds.
- [ ] `gen_dag.py` + `run.sh` (intra-node sweep → `results.csv`).
- [ ] Baseline ports (clone WordCount trees): RMMap-ES, Faasm-like, Cloudburst-Redis; checksum gate matches ours.
- [ ] `plot.py` (StateSync palette / bars conventions, per the runbook §6).
- [ ] Inter-node variant: `gen_variants.py` support + cluster sweep (sibling suite §11) — budget gather-stability work (shuffle = the many-to-many path).

> **Backup workload: PageRank.** If a graph/iterative workload is wanted instead of
> (or after) TeraSort, PageRank is the documented alternative: per-iteration
> scatter-to-neighbors → aggregate → rank update, reusing ML-training's
> iteration-unroll pattern + the `Shuffle`/`dispatch` primitives. Higher effort
> (sparse graph kernel, convergence control, irregular/skewed comms) and not native
> in any baseline either. Tracked as the backup in the inter-node plan.
