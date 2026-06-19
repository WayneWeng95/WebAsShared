# Flink StateFun baseline — MediaReview & SocialNetwork

A third comparable system for the streaming-application track: the two
RTSFaaS-derived workloads (**MediaReview**, **SocialNetwork**) re-expressed on
**Apache Flink Stateful Functions (StateFun)**, alongside **WebAsShared** (our
framework) and the **RTSFaaS** baseline.

It is wire-compatible with — and derived from — the StateFun *transactions*
system vendored at `../../../../../compare_system/flink-statefun-transactions`,
specifically its `statefun-python-ycsb-example` (the `original`/`sagas`/`tpc`
account-transfer workload deployed on k8s + Kafka). We reuse that system's exact
deployment shape (remote Python functions behind the StateFun runtime, Kafka
ingress/egress, Strimzi on k8s) and swap in the two streaming workloads.

## Why StateFun is the right comparable

StateFun's unit is a **keyed stateful function** `(FunctionType, id)` — a keyed
state partition addressed by message. That is precisely the streaming model the
RTSFaaS apps assume and that WebAsShared implements over the SHM page-chain /
RDMA: `Input(event stream) → partition by key → stateful op per partition →
Output`. Each state table becomes a per-key `State`; cross-key access becomes a
message hop between partitions. That hop is the StateFun analogue of the
cross-node state movement WebAsShared optimizes — so the comparison lands on the
metric we care about (state-transfer cost), not on incidental stack differences.

## Provenance

| File | Derived from |
|------|--------------|
| `functions/*.py` (remote function + aiohttp handler shape) | `statefun-python-ycsb-example/original/functions/account_function.py` |
| `protobuf/messages.proto` (`Wrapper{request_id, Any}` in / `Response` out) | `statefun-python-ycsb-example/original/protobuf/messages.proto` |
| `module.yaml`, `docker/Dockerfile.*`, `deployment/k8s/*` | the same example's `module.yaml` / `docker/` / `deployment/k8s/` |
| `driver/stream_bench.py` (Kafka producer + measure) | `evaluation/utils/{producer,measure}.py` |

## Architecture

```
gen_events.py stream ──▶ Kafka topics ──▶ routable-protobuf-ingress
  (../../gen_events.py)   mr_*/sn_*         (key = row id ⇒ function id)
                                                │
                          ┌─────────────────────┴─────────────────────┐
                MediaReview: mediareview_function       SocialNetwork: socialnetwork_function
                  state = {password, rate, review}        state = {password, profile, tweet}
                  login READ / rate WRITE / review WRITE   login/profile single-key;
                  (single-key)                             timeline/post fan out to key+1
                                                │           (cross-partition state access)
                                                ▼
                                   generic-egress ──▶ `responses` topic ──▶ stream_bench gate / throughput
```

- **MediaReview** — `login` compares `user_pwd.password`; `rate`/`review` write
  `movie_rating`/`movie_review`. Each event is a single-key access.
- **SocialNetwork** — `login`/`profile` are single-key reads; `getTimeLine` and
  `postTweet` touch **key and key+1**. The second key is reached by
  `context.pack_and_send(...)` to the key+1 partition *inside the dataflow*, and
  each accessed key emits its own `Response`. So the measure tallies **2** reads
  per `timeline` and **2** writes per `post` — matching the WebAsShared gate.

## Correctness gate (identical to the WebAsShared run)

`driver/stream_bench.py gate` consumes `responses` and tallies 200s by op. The
expected counts come from the SAME deterministic stream (`../../gen_events.py`,
fixed seed), so the gate is line-for-line comparable. Verified at 20 k events:

| workload | gate |
|----------|------|
| MediaReview   | `total_events=20000`, `login_ok == #login` |
| SocialNetwork | `login_ok=5096`, `profile_reads=5075`, `timeline_reads=9758 (=2×#timeline)`, `tweet_writes=9900 (=2×#post)` |

(The SocialNetwork numbers above are the exact counts `gen_events.py
socialnetwork --events 20000 --users 10000` produces — see the parent README.)

## Single-machine run (no k8s) — verified

The whole stack runs on one machine via `docker-compose.yml` (zookeeper + kafka +
StateFun master/worker + the two function containers). No Strimzi/k8s needed.

```bash
sudo ./run-local.sh mediareview 20000 10000      # build, up, seed, replay, gate, down
sudo ./run-local.sh socialnetwork 20000 10000
KEEP=1 sudo ./run-local.sh mediareview            # leave the stack up
THROUGHPUT="400 600" sudo ./run-local.sh mediareview
```

`run-local.sh` builds the four images, brings up the compose stack, waits for the
Flink job via the REST API (`localhost:8081/jobs`), then drives the gate from a
one-shot producer container on the compose network. Needs `docker` (+ `curl` on
the host for the readiness poll); run under `sudo` if your user is not in the
`docker` group (`DOCKER=...` overrides the docker command).

**Verified end-to-end on a single machine (20 k events, users=10000):**

```
MediaReview    total_events=20000  login_ok=4007  rate_writes=8150  review_writes=7843   GATE: PASS
SocialNetwork  total_events=20000  login_ok=5096  profile_reads=5075
               timeline_reads=9758 (=2x timeline)  tweet_writes=9900 (=2x post)          GATE: PASS
```

Both match the WebAsShared gate exactly, and the SocialNetwork run exercises the
real 2-key cross-partition fan-out (`timeline`/`post` hop to the key+1 partition).

## Build & deploy on a cluster

```bash
# 1. Build the four images (regenerates messages_pb2.py via protoc).
REGISTRY=<your-registry> ./build-images.sh
#    -> streaming-example-{statefun,mediareview,socialnetwork,producer}
#    (update image: names in deployment/k8s/*.yaml if you push to a registry)

# 2. Deploy on k8s (Strimzi Kafka operator must be installed).
kubectl apply -f deployment/k8s/kafka.yaml
kubectl apply -f deployment/k8s/topics.yaml
kubectl apply -f deployment/k8s/flink-statefun.yaml
kubectl apply -f deployment/k8s/mediareview-function.yaml
kubectl apply -f deployment/k8s/socialnetwork-function.yaml
```

## Run (against a reachable broker)

```bash
# Correctness gate (reuses ../../gen_events.py for the stream):
BROKER=<host:nodePort> ./run.sh mediareview 200000 10000
BROKER=<host:nodePort> ./run.sh socialnetwork 200000 10000

# Gate + throughput ramp (headline events/s, comparable to RTSFaaS / WebAsShared):
BROKER=<host:nodePort> THROUGHPUT="400 600 800" ./run.sh mediareview 200000 10000

# Run the driver from the producer image instead of local python:
DRIVER=docker BROKER=<host:nodePort> ./run.sh socialnetwork
```

`run.sh` seeds the key space, replays the event stream for the gate, then
optionally ramps a target throughput and reports achieved events/s from the
`responses` topic timestamps (same method as `evaluation/utils/measure.py`).

## Scoping (kept honest, same as the RTSFaaS baseline)

We re-implement each app's **dataflow semantics** on StateFun and compare on
**throughput / latency / state-transfer cost** — *not* transactional isolation.
StateFun's `sagas` / `tpc` handlers exist (and the vendored repo studies them),
but the RTSFaaS app config runs with `abortRatio=0`, so we drive the plain
keyed-function path; we do not add cross-key atomicity/aborts. State this in the
paper so the three-way comparison (WebAsShared / RTSFaaS / Flink StateFun) is
apples-to-apples on state movement.

## Notes / gotchas

- `protobuf` is pinned to **3.20.3** (not the ycsb example's 3.14.0): the
  committed `messages_pb2.py` is generated by a modern `protoc` (builder-style,
  needs protobuf ≥3.20). This still satisfies the StateFun SDK's
  `protobuf>=3.11.3,<4.0.0`. If you regenerate with an older `protoc`, you may
  drop the pin back.
- Topics are **workload-prefixed** (`mr_*` / `sn_*`) so both functions share one
  deployment without colliding on the common op names; `responses` is shared.
- The ingress routes by **Kafka record key = function id** (the row key), so the
  producer sets `key=<row key>` on every record.
- The function images serve the aiohttp app with **`-k aiohttp.GunicornWebWorker`**
  (the vendored ycsb Dockerfile omitted the worker class; the default sync WSGI
  worker fails with `__call__() takes 1 positional argument but 3 were given`).
- A failing invocation fails its whole StateFun **batch** (egress sends roll back
  and the batch retries) — so a bug in one op (e.g. `timeline`) can zero out the
  responses of other ops batched with it. Re-create topics / use a fresh stack
  for a clean gate.
