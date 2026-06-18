# Streaming Application Benchmark — WebAsShared vs Sagas (Flint) / RTSFaaS

**Status: NOTE / planning (added 2026-06-18).** Records the comparison set for the
streaming-application track. Distinct from the streaming *feature* tests
(`../Streaming/`, `../Streaming_CrossNode/`), which validate the `StreamPipeline`
primitive's correctness — this track is an **application-level** evaluation.

## Comparison set

We evaluate **WebAsShared** (stateful streaming DAG over the zero-copy SHM page-chain
intra-node / serialization-free RDMA inter-node) against the two streaming systems it
is closest to:

| System | What it is | Role |
|--------|-----------|------|
| **RTSFaaS** | Real-time serverless; transactional state store (TiKV, lease/TSTREAM concurrency control, Java). **Source of the two workloads.** | baseline |
| **Sagas (built on Flint)** | Saga-pattern stateful streaming on the **Flint** stream-processing engine. | baseline |

> **TODO — pin the Sagas/Flint reference.** Confirm the exact paper + mechanism for
> "Sagas on Flint" (engine model, state/fault-tolerance approach, how it moves
> inter-stage state) before measuring, the way the application-benchmark suite pins
> RMMap/Faasm/Cloudburst. Do **not** guess its internals in the writeup.

## Workloads (both from the RTSFaaS paper)

1. **MediaReview** — a stream of user-interaction events over 3 keyed state tables
   (`user_pwd` / `movie_rating` / `movie_review`); events are `userLogin` /
   `ratingMovie` / `reviewMovie`, each a single-key access. Stress knobs:
   multi-partition ratio (cross-partition access), state-access skew (hot keys).
2. **SocialNetwork** — the second RTSFaaS app (event-stream social-graph operations).
   *Spec to extract from the RTSFaaS source (see below).*

Original RTSFaaS configs are preserved as the spec under
`../../Benchmarks/RTSFaaS/{MediaReview,SocialNetwork}/` (`driver.sh`/`worker.sh`,
`Env/*.env`); the application logic is Java in the RTSFaaS repo (not reproduced).

## How we run them (re-implement the app, not the stack)

We **do not** reproduce RTSFaaS's transactional stack (TiKV + TSTREAM + Java). We
re-implement each app's **dataflow semantics** on our stream-processing model and
compare on **throughput / latency / state-transfer cost** — not transactional
isolation (we don't implement TSTREAM/aborts; `abortRatio=0` in the app config
anyway). State this scoping explicitly in the paper so the comparison is honest.

The MediaReview → our-primitives mapping is already worked out in detail in
[`../../Benchmarks/WORKLOAD_SELECTION.md`](../../Benchmarks/WORKLOAD_SELECTION.md)
(§"MediaReview on our stream-processing model"):

```
Input (event stream) → Shuffle (partition by key) → stateful op per partition → Output
                       routing/shuffle.rs           StreamPipeline + shared_area/atomic_arena
```

- **State tables → SHM keyed state**, sharded across nodes by the shuffle (a node
  owns a key range).
- **Multi-partition ratio → cross-node state-access ratio** (RemoteSend/Recv or
  one-sided RemoteAtomic over RDMA) — exactly the state-transfer cost we optimize.
- **Skew → shuffle imbalance** (Zipfian key generator → hot partitions).

SocialNetwork maps the same way (Input → Shuffle-by-key → stateful op → Output);
its event mix + state tables come from the RTSFaaS source.

## Metrics

Event throughput (events/s) vs multi-partition ratio and skew; end-to-end latency
(median + tail); per-node SHM / billable memory; **RDMA bytes** (cross-node state
access — the headline vs the baselines' serialized state movement). The first two
sweeps mirror RTSFaaS's own knobs so curves are shape-comparable.

## Suggested artifacts to add (when we build it)

- `Executor/guest/src/workloads/media_review.rs` (`mr_dispatch`/`mr_login`/`mr_rate`/
  `mr_review`) + a Zipfian key-skew event generator; similar for SocialNetwork.
- `DAGs/symbolic_dag/media_review_auto_placement.json` + an RDMA two-node variant
  under `DAGs/rdma_workload_dag/` (input → shuffle → stateful → output).
- `run.sh` / `plot.py` in this folder, shared column shape + StateSync palette.

## Status

- [ ] Pin the Sagas/Flint paper + mechanism (and RTSFaaS run recipe / how baselines are driven).
- [ ] Extract the SocialNetwork spec (event mix + state tables) from the RTSFaaS source.
- [ ] `media_review.rs` guest + event generator; MediaReview DAGs.
- [ ] SocialNetwork guest + DAGs.
- [ ] Sweeps (multi-partition ratio × skew) + figures; throughput/latency/RDMA-bytes vs both baselines.

## References

- Workload specs + mapping: `../../Benchmarks/WORKLOAD_SELECTION.md`, `../../Benchmarks/RTSFaaS/`.
- Streaming primitive (what we build on) + its feature tests: `../Streaming/`, `../Streaming_CrossNode/`.
- Sibling application-benchmark suites (conventions, fairness, plotting): `../Intra-Node Application_Benchmark/`, `../Inter-Node Application_Benchmark/`.
