# RTSFaaS baseline — frozen source for MediaReview & SocialNetwork

Preserved, unmodified copies of the two RTSFaaS workloads we compare against,
brought in as the **baseline spec** (the way `../../Intra-Node
Application_Benchmark/Finra/baseline/` preserves RMMap/Faasm/Cloudburst). We do
**not** reproduce the RTSFaaS stack (TiKV + TSTREAM lease concurrency control +
Java) — these files are the authoritative description of the *application
semantics* we re-implement on our stream model.

Provenance:

| File | Copied from |
|------|-------------|
| `MediaReview/MediaReview.java`   | `compare_system/RTSFaaS/morph-clients/src/main/java/client/MediaReview.java` |
| `SocialNetwork/SocialNetwork.java` | `compare_system/RTSFaaS/morph-clients/src/main/java/client/SocialNetwork.java` |
| `{MediaReview,SocialNetwork}/{driver,worker}.sh`, `Env/*.env` | `Benchmarks/RTSFaaS/` (the original run recipe) |

## What the apps are (extracted from the source)

**MediaReview** — `defineFunction()` registers one DAG `mediaReview` with three
single-key functions over three keyed tables:

| function | access | table → value | param |
|----------|--------|---------------|-------|
| `login`  | READ   | `user_pwd.password` | compares input password |
| `rate`   | WRITE  | `movie_rating.rate` | writes new rating |
| `review` | WRITE  | `movie_review.review` | writes new review |

Sizing from `Env/MediaReview.env`: tables `10000;10000;20000` items, value sizes
`16;64;64` B, `ratioOfMultiPartitionTransactionsForEvents=100`,
`stateAccessSkewnessForEvents=1`, `abortRatioForEvents=0`.

**SocialNetwork** — four DAGs: `userLogin` (READ `user_pwd`), `userProfile`
(READ `user_profile`), `getTimeLine` (READ `tweet` ×2 keys), `postTweet` (WRITE
`tweet` ×2 keys). `Env/SocialNetwork.env`: `eventRatio=0;0;0;100` (the shipped
config drives `postTweet` only), 2-key access, skew on the tweet table.

## How we re-implement them (our framework)

`Input(event stream) → Shuffle(partition by key) → stateful op per partition →
Output`, with the three/four tables held as **keyed SHM state**
(`insert_shared_data` / `read_shared_chain`). See the parent
[`../../README.md`](../README.md) and `WORKLOAD_SELECTION.md`
(§"MediaReview on our stream-processing model") for the full mapping. The
re-implementation lives in:

- `Executor/guest/src/workloads/media_review.rs`, `social_network.rs`
- `DAGs/symbolic_dag/{media_review,social_network}_auto_placement.json`
- `../gen_events.py`, `../run.sh`

We compare on **throughput / latency / per-node SHM / RDMA bytes** — *not*
transactional isolation (`abortRatio=0` anyway). That scoping is intentional and
stated in the parent README.
