# RMMap (dmerge) WordCount baseline — ES protocol on this single box

RMMap ships WordCount natively (`compare_system/RMMap/exp/wordcount/`). Its app
runs `splitter → mapper×N → reducer` as **Knative Eventing** over the dmerge
runtime, with the transport chosen by `PROTOCOL`:

| `PROTOCOL` | Transport | |
|-----------|-----------|---|
| `DMERGE` | RDMA remote-memory-map, **serialization-free** (kernel module) | the headline / our closest twin |
| `ES` | **Redis** (`redis_put`/`redis_get`) + `pickle` | the **serialized** baseline — *what runs here* |

## What runs here (ES) and why it's possible without the cluster

This box has live RDMA (`mlx4_0`, RoCE) and is kernel-module-capable, but per the
agreed scope we run **ES only — no kernel module**. ES needs neither RDMA nor the
MITOSIS module: `util.py` sets `SD = sopen() if PROTOCOL in ('DMERGE',...) else 0`,
so for ES no module syscall runs.

The blocker for *any* RMMap run is normally that the app image
(`val01:5000/dmerge-wordcount`) descends from a **private custom-CPython** built
from an SJTU-internal GitLab (`Dockerfile-cpython-build`). **Bypassed:** the dmerge
`bindings` Cython module (`pyx/bindings.pyx` + `native/wrapper.h`, self-contained C)
**builds and imports on stock `python:3.7` with no kernel module** — verified. The
custom CPython is only needed for DMERGE's heap-malloc path.

So this baseline is a small self-contained image (`Dockerfile`, stock python:3.7)
that builds the bindings and runs RMMap's **real ES handlers**.

### How the DAG is driven (no Knative)

Knative Eventing isn't stood up here, so `driver.py` replicates the
splitter→mapper×N→reducer chain by **calling RMMap's real `functions.splitter` /
`mapper` / `reducer` (ES branch) directly** inside the container, threading state
through real Redis with real `pickle`. Faithful: the ES data path and its profile
(`execute_time` / `es_time` = Redis round-trip / `sd_time` = pickle ser-deser) —
the serialized inter-stage transfer we contrast with our zero-copy page-chain.
Dropped: the HTTP/CloudEvent envelope (network `nt_time`) and Knative placement —
neither is the serialization cost being compared.

### Minimal edits to the copied app (`app/`)

1. **`functions.py` splitter** — corpus path from `WC_CORPUS` env (the WordCount
   README sanctions pointing all systems at the same `TestData/corpus_*`).
2. **`util.py`** — Redis host/port/password from env (was hardcoded `'redis'`).
3. **`functions.py` mapper — correctness fix:** upstream wrote every mapper's
   partial to the **same** key `/tmp/wc.txt`, so with a shared KVS the partials
   clobber and the reducer sums the survivor N times. Keyed by mapper `ID`
   (`/tmp/wc_<i>.txt`) so the N partials are distinct (DMERGE already keys by
   unique object id). Timing (`es`/`sd`) unaffected; **counts now correct**.

## Run

```bash
# builds the image, starts a dedicated Redis container, sweeps N
./run.sh corpus_large.txt "1 2 4 8 16"               # → results.csv
MAPPER_PARALLEL=1 ...                                 # parallel mappers (default in the sweep)
```

## Parallel mappers (default) vs sequential

The real RMMap deploys each mapper as a **separate Knative pod**, so the mappers
run concurrently. `driver.py` therefore runs them as **concurrent processes**
(`MAPPER_PARALLEL=1`, spawn — each its own process + Redis connection, like a
pod). `results.csv` holds these parallel numbers; `results_sequential.csv` keeps
the original single-process run for reference.

> **Why this matters (the Redis-bottleneck question).** RMMap's ES splitter writes
> the **whole corpus to one Redis key**, and **every mapper `redis_get`s and
> `pickle.loads` the entire corpus**, using only its 1/N slice (`functions.py`
> `splitter_es` L88-92, `mapper_es` L189-193 — verified against the original).
> So Redis traffic = **N× corpus** and grows with fan-out. Sequentially that
> cost just adds to the Python-regex floor; **run in parallel it becomes the
> ceiling** — N mappers hammering one Redis. That is the inherent ES baseline
> RMMap's DMERGE (serialization-free RDMA) is designed to beat.

## Results (2026-06-12, single box, **parallel mappers**)

| size | best MB/s | at N | shape |
|------|-----------|------|-------|
| 50 MB | **16.6** | 8 | 7.3→10.9→14.5→16.6→13.9 — rises, then N=16 drops as Redis saturates |
| 500 MB | **24.0** | 8 | 7.8→13.2→19.7→24.0→17.9 |
| 1 GB | **24.3** | 8 | 7.7→13.4→20.3→24.3→17.9 |

- **Counts** `8940339 / 89403388 / 179060000` = our system & Cloudburst → validated.
- **Peaks at N=8, drops at N=16**: past 8 mappers, the N× whole-corpus broadcast
  **saturates the single Redis** — summed `es_ms` explodes (500 MB N=16 = 180 s,
  1 GB N=16 = 380 s of Redis time across the mappers). Redis is the bottleneck,
  exactly as expected for the broadcast design.
- vs **sequential** (`results_sequential.csv`): ~3× lower and a *declining* curve
  — that was a single-process artifact, not RMMap's behavior.
- Our zero-copy page-chain reports the serialization/Redis cost as **0**; DMERGE
  (parked) is RMMap's own serialization-free answer.

> **Cross-system fairness note.** These RMMap-ES numbers are now **parallel**
> (matching its pods). The **Cloudburst** baseline is still **single-process** in
> its local runner, so the overlay compares parallel RMMap-ES vs sequential
> Cloudburst — not apples-to-apples on the parallelism axis. Parallelize the
> Cloudburst runner too (its real deployment also fans mappers across executors)
> before drawing system-vs-system conclusions from the absolute curves.

## Caveats / scope

- **ES only.** `DMERGE` (the RDMA, serialization-free headline — our closest twin)
  needs the MITOSIS **Rust kernel module** built+`insmod`'d against this kernel
  (7.0.0); parked per the no-kernel-module scope. The box *does* have RoCE
  (`mlx4_0`) and module-load capability, so DMERGE is attemptable later.
- No Knative HTTP/eventing layer (see above) — `nt_time` not measured.
- Don't compare against RMMap's published numbers (different cluster/RDMA).
