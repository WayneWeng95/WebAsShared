# TeraSort — WebAsShared vs RMMap / Faasm / Cloudburst  *(workload #7)*

**Status: BUILT + RUN intra-node, all four systems (2026-06-18).** New data-parallel
workload that adds the one structure the current five lack: an **all-to-all
shuffle**. Runs in **both** suites — intra-node (this folder, SHM page-chain, DONE)
and inter-node (`../../Inter-Node Application_Benchmark/`, RDMA page-chain, pending).

## Results (intra-node, this box — 2026-06-18)

Deterministic 100-byte Gensort records (uniform keys over 64 symbols → balanced
ranges); **unified size axis {50, 250, 500 MB}** × fan-out N {1,2,4,8,16}, all
four systems. The key-multiset checksum `<records>:<keysum>` is **identical across
every N and every system** (50 MB = `524288:338112585`, 250 MB =
`2621440:1690717118`, 500 MB = `5242880:3381554924`; 60/60 cells pass) — the
all-to-all shuffle drops no records. Peak throughput (MB/s, best over N):

| size | WasMem (ours, AOT) | Cloudburst | RMMap-ES | Faasm-like | ours speedup |
|------|--------------------|------------|----------|------------|--------------|
| 50 MB  | **168** | 21 | 21 | 20 | **8.1×** |
| 250 MB | **183** | 18 | 19 | 21 | **8.7×** |
| 500 MB | **205** | 19 | 19 | 21 | **9.6×** |

Ours (5 reps) scales 70→205 MB/s with N; the baselines (3 reps, single-process
Redis) stay flat ~18–21 — the per-N speedup slope isn't meaningful for them
(single-process caveat), so compare absolute throughput and the KV byte volume.
1 GB also runs intra-node on our side (N≥4, ~123 MB/s — see the SHM-window note
below); the baselines were swept on the shared 50/250/500 MB axis.

**Shuffle bytes through the external store (the headline):** ours **0×** (records
cross the shuffle zero-copy via the host Aggregate page-chain splice); baselines
serialize the **whole dataset** through Redis — RMMap-ES ≈ 4× input (pickle),
Cloudburst ≈ 6× (3× put + 3× get, cloudpickle), Faasm-like ≈ 4× (chunk+bucket
state). TeraSort moves the entire input across the exchange, so this gap is the
largest of the suite. Plot: [`figs/terasort_overlay.pdf`](./figs/terasort_overlay.pdf).

### Fitting big inputs in the SHM window (the 1 GB OOM, fixed)

The guest's wasm linear memory is split: the host maps the SHM file at
`TARGET_OFFSET = 2 GiB`, so **record data lives in the 2 GiB window above it and
the guest heap in the 2 GiB below** — not one 4 GiB pool. The first cut OOM'd 1 GB
at *every* N because (a) `ts_partition`/`ts_merge` did `read_all_stream_records`,
copying a full shard/range into the **heap** (low-N OOM), and (b) the partition
`append`-copies the whole dataset into per-owner sub-slots while the input shards
are still resident → **2× input in the SHM window** (high-N capacity trap). The
routed copies cost ~1× input regardless (they feed the Aggregate), so 1× is the
SHM floor; the fix is to never *also* hold the inputs or a second heap copy:

- **Stream, don't `read_all`** — `for_each_stream_record` walks the page chain one
  record at a time (heap bounded by one record), used in both `ts_partition`
  (route) and `ts_merge` (collect keys only, ~10% of the range, then sort).
- **Free shards as we go** — per-shard `FreeSlots` (one per `ts_partition`), and
  for large inputs (>600 MB) run the partitions **serially** so each freed shard's
  pages are recycled into the routed copies → SHM stays ≈ 1× input, not 2×
  (`gen_dag.py` `PART_GROUP`; small inputs keep full concurrency).

Result: **1 GB now runs at N ≥ 4** (≈123 MB/s); N = 1, 2 still OOM (too few
shards to recycle — the whole input + its copy must briefly coexist), an honest
and expected low-N limit. The same changes also cut 500 MB peak memory ~1.9 GB →
~1.3 GB and roughly *doubled* throughput at 50/500 MB (streaming removed the full
heap copies). Built with [zero-copy aggregation + page-chain frees] — no wasm64.

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

## Step-by-step

- [x] `gen_records.py` — deterministic 100-byte Gensort records (key + 64-symbol uniform key); sizes 50/500/1000 MB.
- [x] Guest `terasort.rs` (`ts_distribute` / `ts_partition` / `ts_merge`) — additive, no edits to existing stages. **Range-partition by key-prefix in `ts_partition`; the host `Aggregate` ×N is the zero-copy shuffle** (the whole-stream `Shuffle` primitive routes per-stream, so key routing lives in the guest and the per-owner gather uses Aggregate). Built via `./build.sh` (exports verified).
- [x] `gen_dag.py` emits the native single-node DAG: `load → ts_distribute → ts_partition×N → Aggregate×N → ts_merge×N → Output×N`. (No `_auto_placement.json` yet — that's for the inter-node cluster sweep.)
- [x] Validate our side: `node-agent run` smoke → every range sorted, ranges globally ordered, checksum gate holds.
- [x] `run.sh` (intra-node sweep → `results_aot.csv`); fan-out-invariant checksum + sorted + global-order gates enforced per run.
- [x] Baseline ports (clone WordCount trees): **Cloudburst-Redis** (`baseline/cloudburst/`, real `RedisCloudburst` DAG runner), **RMMap-ES** (`baseline/rmmap/`, ES pickle+Redis data path; Docker unusable here so run direct), **Faasm-like** (`baseline/faasm/demo/`, `ts.rs`→wasm32-wasip1 Faaslets, state through Redis). **Checksum matches ours at every N and size.** Baselines isolated on separate Redis DBs (cloudburst db0, rmmap db2, faasm db3) so concurrent runs don't `flushdb` each other.
- [x] `plot.py` → [`figs/terasort_overlay.pdf`](./figs/terasort_overlay.pdf) (StateSync palette: throughput vs N, peak throughput per size, shuffle bytes through the KV).
- [x] **1 GB intra-node OOM fixed** via `for_each_stream_record` (bounded-heap streaming) + per-shard `FreeSlots` + serial partition for >600 MB (`gen_dag.py` `PART_GROUP`). 1 GB now runs at N≥4 (~123 MB/s); also ~2× faster at 50/500 MB. No wasm64.
- [ ] Inter-node variant: `gen_variants.py` support + cluster sweep (sibling suite §11) — budget gather-stability work (shuffle = the many-to-many path). 1 GB at N=1,2 (where the input+copy can't both fit one node's SHM window) is a natural fit for spreading the dataset across nodes.

> **Backup workload: PageRank.** If a graph/iterative workload is wanted instead of
> (or after) TeraSort, PageRank is the documented alternative: per-iteration
> scatter-to-neighbors → aggregate → rank update, reusing ML-training's
> iteration-unroll pattern + the `Shuffle`/`dispatch` primitives. Higher effort
> (sparse graph kernel, convergence control, irregular/skewed comms) and not native
> in any baseline either. Tracked as the backup in the inter-node plan.
