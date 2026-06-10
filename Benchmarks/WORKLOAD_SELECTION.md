# Workload Selection for Cross-System Comparison

Goal: pick **7 real application workloads** (not micro-benchmarks) drawn from the five
comparison systems, to evaluate WebAsShared against. Selection criteria: (1) **direct
comparability** to our design claims, (2) **representativeness** across the workload space,
(3) **porting cost** onto our model.

See [`benchmark_workloads.md`](./benchmark_workloads.md) for the full per-system inventory and
[`MICROBENCHMARKS.md`](./MICROBENCHMARKS.md) for what was deliberately excluded.

---

## Our system in one line

WebAsShared is a **WASM DAG dataflow engine** with a zero-copy shared-memory page-chain substrate
and **RDMA serialization-free state transfer** between DAG stages. It already ships native
workloads for `word_count`, `tfidf`, `finra`, `ml_training`, and `img_pipeline` (WASM + PyFunc).

Because of that, the five systems are **not equidistant** from us:

| System | Overlap with us | Porting implication |
|--------|-----------------|---------------------|
| **RMMap (dmerge)** | Highest — RDMA remote-memory-map, ser/deser-free DAG state transfer. Same thesis. | Apples-to-apples; apps are Python DAGs → easy. |
| **Faasm** | High — WASM/Faaslet isolation + shared-memory state; "billable memory" metric. | Carries the footprint argument. |
| **Roadrunner** | High on transport — zero-copy data delivery to WASM functions. | Directly stresses our page-chain + RDMA claim. |
| **Cloudburst** | Medium — stateful serverless, but via Anna KVS (not RDMA/SHM). | Good for ML-serving + distributed-compute axes. |
| **RTSFaaS** | Lowest natively — transactional store (TiKV, lease CC, Java). | Different paradigm; we model it as a stateful stream job (see below). |

---

## Similarity clustering (the application workloads collapse into ~6 families)

1. **ML inference / image classification** — RMMap `digital-minist` (MNIST), Cloudburst
   `mobilenet`+`predserving`, Faasm `tflite-inference`. *Present in 3 systems → strongest cross-system axis.*
2. **Distributed ML training** — Faasm `ml-training-sgd` (HOGWILD!), RMMap `ml-pipeline`. *We have `ml_training`.*
3. **Distributed matrix multiply** — Cloudburst `summa`, Faasm `matrix-multiply`.
4. **Data-parallel text processing** — RMMap `wordcount` (+Java). *We have `word_count`.*
5. **Multi-stage stateful pipeline** — RMMap `finra` (we have `finra`), Roadrunner
   `image-resize`/`fanout` (we have `img_pipeline`).
6. **Transactional / stateful event stream** — RTSFaaS `MediaReview`, `SocialNetwork`.

## Porting cost

| Tier | Workloads | Why |
|------|-----------|-----|
| **Trivial** (native already) | WordCount, FINRA, ML-training, TF-IDF, Image pipeline | Already run; just match datasets/sizes. |
| **Moderate** | MNIST inference, Matrix-multiply, MediaReview-as-stream | New guest function(s), data fits our model. |
| **Hard** | TF-Lite/MobileNet in-Wasm (Faasm) | Needs a tflite runtime inside WASM; Cloudburst's Python `mobilenet` is the cheaper route. |
| **Very hard** | RTSFaaS run *as-is* (TiKV + lease CC + Java) | We don't reproduce their stack; we re-implement the *application* on our stream model instead. |

---

## Final selected set (7 workloads, all 5 systems)

| # | Workload | From | Family | Why it earns a slot |
|---|----------|------|--------|---------------------|
| 1 | **WordCount** | RMMap | data-parallel | Universal baseline; RMMap is our closest twin → cleanest head-to-head on ser/deser-free transfer. Native. |
| 2 | **FINRA** | RMMap | multi-stage stateful DAG | Realistic financial pipeline with inter-stage state — exactly what page-chain splicing targets. Native. |
| 3 | **ML training (HOGWILD! SGD)** | Faasm | distributed training | Faasm is WASM like us; carries the **billable-memory / footprint** argument. Native. |
| 4 | **MobileNet image classification** | Cloudburst | ML inference | The axis present in 3 systems → most representative. Cloudburst's Python version is the cheapest port. |
| 5 | **Image-resize / fanout** | Roadrunner | data delivery | Roadrunner's whole point is zero-copy delivery to WASM — validates our transport claim. Maps to `img_pipeline`. |
| 6 | **MediaReview** | RTSFaaS | stateful event stream | Re-implemented on our stream-processing model (below). Adds a transactional/streaming app and covers the 5th system. |
| 7 | **Matrix multiply** | Faasm | distributed compute | Divide-and-conquer recursive chaining → maps directly to our fan-out + aggregate routing; heavy cross-node block transfer stresses RDMA. |

Coverage: RMMap ×2, Faasm ×2, Cloudburst ×1, Roadrunner ×1, RTSFaaS ×1 — **all five systems**.

> Source choice for #7: Faasm's `matrix-multiply` (WASM divide-and-conquer, recursive chaining)
> keeps it WASM-vs-WASM and maps onto our fan-out/aggregate primitives. The alternative is
> Cloudburst's `summa` (2D block algorithm over Anna KVS) — swap it in for a named distributed-matmul
> and a second Cloudburst data point instead of a second Faasm one.

---

## MediaReview on our stream-processing model

RTSFaaS runs MediaReview as a transactional workload (TSTREAM concurrency control, TiKV, Java).
We **do not reproduce that stack** — we re-implement the *application semantics* as a stateful
streaming DAG and compare on throughput/latency and state-transfer cost. The original config
(`Env/MediaReview.env`, `MediaReview/{driver,worker}.sh`) is preserved here as the spec.

### What the app is

A stream of user-interaction events over **3 keyed state tables**:

| Table | Items | Value | Event that touches it | Mix |
|-------|-------|-------|-----------------------|-----|
| `user_pwd` | 10 000 | 16 B | `userLogin` (verify password) | 20% |
| `movie_rating` | 100 000 | 16 B | `ratingMovie` (update rating) | 40% |
| `movie_review` | 100 000 | 256 B | `reviewMovie` (append review) | 40% |

Each event is a single-key access (`keyNumberForEvents=1`). Stress knobs:
`ratioOfMultiPartitionTransactionsForEvents` (cross-partition access), `stateAccessSkewnessForEvents`
(hot keys), `abortRatioForEvents` (0 in the app config). `CCOption=3` (TSTREAM), `isRDMA=1`.

### Mapping to our primitives

```
Input (event stream)
   │   records = {eventType, key, value}
   ▼
Shuffle (partition by key)              ← routing/shuffle.rs, Modulo/FixedMap policy
   │   keyed state tables sharded across nodes
   ▼
Stateful op per partition               ← StreamPipeline stage + shared_area / atomic_arena
   │   userLogin  : read user_pwd[key], compare
   │   ratingMovie: update movie_rating[key]
   │   reviewMovie: append movie_review[key]
   │   cross-partition event → RemoteRecv / RemoteAtomic on the owning node (RDMA)
   ▼
Output (per-event result / committed count)
```

- **State tables → SHM keyed state.** Hold the 3 tables in `shared_area` (or `atomic_arena` for
  the counter-like rating updates), partitioned across nodes by the shuffle. A node owns a key range.
- **Multi-partition ratio → cross-node access ratio.** When an event's key lives on another node,
  service it via `RemoteSend`/`RemoteRecv` or a one-sided `RemoteAtomicFetchAdd`/`CmpSwap`. The
  `ratioOfMultiPartitionTransactionsForEvents` knob becomes the fraction of events that trigger an
  RDMA state access — this is exactly the state-transfer cost our design optimizes.
- **Skewness → shuffle imbalance.** `stateAccessSkewnessForEvents` drives a Zipfian key generator;
  feeds straight into the shuffle and exposes hot-partition behavior.
- **Concurrency control → out of scope (intentionally).** We don't implement TSTREAM/aborts
  (`abortRatio=0` in the app anyway). We measure the **dataflow** cost: event throughput, end-to-end
  latency, and cross-node state-access (RDMA) volume — not transactional isolation. State this
  scoping explicitly in the paper so the comparison is honest.

### Metrics to report (aligned with our other workloads)
Event throughput (events/s) vs. multi-partition ratio and skew; end-to-end latency; per-node SHM /
billable memory; RDMA bytes transferred. The first two sweeps mirror RTSFaaS's own knobs so the
curves are comparable in shape even though the substrate differs.

### Suggested artifacts to add
- `Executor/guest/src/workloads/media_review.rs` — the stateful guest functions
  (`mr_dispatch`/`mr_login`/`mr_rate`/`mr_review`) plus a key-skew event generator.
- `DAGs/symbolic_dag/media_review_auto_placement.json` and an RDMA two-node variant under
  `DAGs/rdma_workload_dag/` — the input→shuffle→stateful→output DAG.
- A small event-stream generator (or reuse RTSFaaS's `eventRatio`/`numberItemsForTables` to size it).
