# Benchmark Test Plan

Cross-system evaluation of **WebAsShared** against the five comparison systems, using the 7
workloads in [`workload_selection_table.tex`](./workload_selection_table.tex) (narrative in
[`WORKLOAD_SELECTION.md`](./WORKLOAD_SELECTION.md)). All comparison-system repos live in
`../compare_system/{roadrunner,RMMap,faasm,cloudburst,RTSFaaS}`.

**Strategy: one workload at a time.** We fully stand up, run, and validate each workload (on our
system *and* its baseline systems) before moving to the next. **Current focus: Image-resize / fanout
(#1), against Roadrunner.** The remaining six are queued as a backlog and detailed when we reach
them.

> **Order note (changed).** This plan now follows the order in `workload_selection_table.tex`:
> **1 Image-resize/fanout (Roadrunner)**, 2 WordCount, 3 FINRA, 4 Matrix-multiply, 5 ML-training,
> 6 ML-inference (MNIST), 7 MediaReview. Image-resize moved to **first** because it most directly
> stresses our headline claim (zero-copy, serialization-free data delivery to WASM functions) and is
> a small, single-baseline (Roadrunner-only) target — the cleanest harness pipe-cleaner.

Per-workload test code lives under [`../Tests/`](../Tests/) (one folder per workload, each with
`run.sh`/`run.py`, `results.csv`, `plot.py`, `figs/`). Image-resize/fanout code:
[`../Tests/ImageResize_Fanout/`](../Tests/ImageResize_Fanout/).

---

## Methodology (applies to every workload)

- **Fairness.** Same physical cluster, same hardware, same input data for our system and every
  baseline. Record node count, CPU, RAM, NIC/RDMA specs once.
- **What we measure** (common metric set — our side comes from `NodeAgent/agent/src/metrics.rs`):
  - **Throughput** (records or items/s, or MB/s delivered) and **end-to-end runtime**.
  - **Latency** — median + tail (p95/p99) where the workload is request-style.
  - **Memory / billable memory** — our `rss_bytes` (summed private RSS of the executor process
    tree) `+ shm_bump_offset` (SHM counted once); baselines use their own peak-memory metric.
  - **RDMA bytes transferred** (cross-node state movement) where applicable.
  - **Serialization cost** — for the data-delivery workload (Roadrunner), report serialization
    throughput/latency explicitly; ours is **zero** (the headline), Roadrunner's WASI/runc baselines
    are not.
  - **Scaling** — sweep node/parallelism (or fanout) count.
- **Staging excluded.** Input staging is timed separately and excluded from compute time (our
  worker already does this).
- **Repeat & report.** ≥5 runs per config; report median + spread. Warm and cold (cold-start) where
  relevant.
- **Comparison scope.** Per-row by default (compare each workload only against the system(s) in its
  table row). Per the new table, workloads **2–5 span three systems** (RMMap / Faasm / Cloudburst),
  workload **6** spans three (RMMap / Cloudburst / Faasm), and workloads **1** (Roadrunner) and **7**
  (RTSFaaS) are single-baseline.
- **Cloudburst backing store = Redis (not Anna KVS).** Anna is available but not widely used, so for
  ease of use every Cloudburst baseline runs against Redis, which Cloudburst supports natively.
  Caveat: Anna's *co-located cache* is part of Cloudburst's data-locality story; with an external
  Redis, locality-sensitive numbers will differ from the published ones. Fine for our throughput /
  memory comparisons, but state it explicitly and don't compare against Cloudburst's Anna-based
  locality results.

---

## Phase 0 — Shared infrastructure (do once, before Workload 1)

- [ ] Lock the cluster: N nodes, RDMA fabric up, record specs (CPU, RAM, NIC/RDMA).
- [ ] Build our system (`./build.sh`) → `node-agent`, `host`, guest WASM.
- [ ] Confirm the metric harness emits throughput / `rss_bytes` / `shm_bump_offset` per run
      (`NodeAgent/agent/src/metrics.rs`, default log `/tmp/node_agent_metrics.jsonl`).
- [ ] Stand up baseline systems *as each workload needs them* (don't deploy all five up front).
      **For Workload 1 that means Roadrunner only** (WasmEdge + containerd shim stack — see below).

---

## Workload 1 — Image-resize / fanout  *(current focus)*

**Compare against:** **Roadrunner** (single baseline).
**Family:** data delivery. **Native on our system:** `Executor/guest/src/workloads/img_pipeline.rs`
(`img_load_ppm → img_rotate → img_grayscale → img_equalize → img_blur → img_export_ppm`).
**Why first:** Roadrunner's whole thesis is zero-copy, serialization-free *data delivery* to WASM
functions — exactly our page-chain + RDMA claim. Smallest, single-baseline target → harness
pipe-cleaner.

### What the Roadrunner workload actually is

Two pieces in `../compare_system/roadrunner/`:

1. **`examples/image-resize/`** — the *application*: a WASM (and container, and native) function that
   resizes an image. Roadrunner's `experiments/motivation/scripts/run_{wasm_,}image_resize.sh`
   measure pull/unpack/run (cold-start/deploy), **not** data delivery — that's a deployment
   microbench, secondary for us.
2. **`examples/fanout-wasm/` + `experiments/evaluation/`** — the *data-delivery* experiment, and the
   one that matters. A **client** function reads a payload and **fans it out to N tasks**
   (`NUM_TASKS`), each calling `func_connect(ptr, len)` to deliver the payload to a server function
   `func_b`. Three transports are compared (their result CSVs: `…/results/parallel/inter-node/
   inter-node-fanout-*.csv`, columns `fanout, roadrunner, runc, wasmedge`):
   - **roadrunner** — zero-copy delivery via `splice`/`vmsplice` (their best mode),
   - **wasmedge** — vanilla WASI (copies through linear memory),
   - **runc** — container baseline.
   Metrics captured per fanout degree: `total_throughput`, `total_latency`,
   `serialization_throughput/latency`, `ram`, `user/kernel/total_cpu`.

> **Roadrunner runs BOTH intra-node and inter-node — and both sequential and parallel (fanout).**
> Their results tree is a full 2×2 (`results/{sequential,parallel}/{intra-node,inter-node}/`):
> *parallel* = the fanout sweep (x-axis = fanout degree 1…100, payload fixed at `file_10M.txt`);
> *sequential* = a single chain (x-axis = payload size 1…500 MB). **Topology is not a separate code
> path** — the same scripts switch intra↔inter purely via env vars: intra = default
> `STORAGE_IP=127.0.0.1:8888` / `FUNCB_URL=http://127.0.0.1:8080/` (storage + `func_b` on localhost),
> inter = override both to a remote node's IP. Their transport columns differ by topology:
> intra-node has `RoadRunner_Embedded` (in-process, their fastest — scales 178→271 over fanout),
> `RoadRunner`, `RunC`, `Wasmedge`; inter-node drops `Embedded` (no in-process meaning across nodes)
> and is network-bound (`roadrunner` ~4.5, `runc` ~10, `wasmedge` ~1.66). **We therefore sweep both
> topologies too:** ours intra-node = single-node SHM page-chain, ours inter-node = 2-node RDMA.

The table row #1 ("one-to-many image-**resizing** data delivery") is the **union**: payload = an
image, fanned **one-to-many** to N **resize** functions. So the apples-to-apples target is the
**fanout experiment with an image payload and a resize `func_b`**, measured against our page-chain /
RDMA delivery.

> **Both Roadrunner pieces are single-step (confirmed from source).** `image-resize` is a one-shot
> (load → `resize(0.5, Nearest)` → encode → exit); `func_b` (`func_b_vanilla/src/main.rs`) just
> *receives* the delivered bytes. The fanout client spawns N tasks that each deliver the **same**
> payload via `func_connect(ptr, len)`. Roadrunner measures the **delivery transport** (zero-copy
> splice vs WASI copy vs container), **not** multi-stage compute. **Consequence for us:** the per-task
> work on our side must be a **single `img_resize` step**, NOT our existing 6-stage `img_pipeline`
> chain — otherwise we'd compare our 6-stage compute against their 1-step delivery and muddy the
> transport claim.

### 1a. Our system — map onto fan-out routing

- **Per-task work — two variants:**
  - **`noop` (pure transport, primary):** an **empty** guest function that receives the delivered
    image and immediately returns (no compute). This isolates **raw delivery cost** and is the
    *truest* apples-to-apples with Roadrunner, whose `func_b` already just receives the bytes and
    returns. Run this first — it's the headline transport number.
  - **`img_resize` (transport + compute, secondary):** a **single** resize step (downscale by a fixed
    factor) added to `img_pipeline.rs`. **Do NOT reuse the 6-stage pipeline** — Roadrunner's per-task
    work is at most one step (see the source note above), so ours must be one step too.
- **Fan-out DAG:** one `Input` (the image) → **broadcast/fan-out** to N parallel `img_resize`
  functions → `Output`/aggregate. Use our routing primitives:
  `Executor/host/src/routing/{broadcast,dispatch,aggregate,shuffle}.rs`. Reference shapes:
  `DAGs/demo_dag/img_pipeline_demo.json` (linear pipeline) and
  `DAGs/symbolic_dag/img_pipeline_auto_placement.json` (2-node RDMA + shuffle).
- **Two configs:**
  - **Intra-node:** single-node fan-out over shared-memory page-chain (zero-copy) —
    `node-agent run DAGs/.../img_fanout.json`.
  - **Inter-node:** 2-node fan-out over RDMA (serialization-free state transfer) —
    `node-agent submit --config NodeAgent/agent_coordinator.toml --dag DAGs/rdma_workload_dag/img_fanout_node*.json`.
- [ ] Author `img_fanout` DAG(s) (single-node + RDMA 2-node) — new under `DAGs/`, parameterised on
      fanout degree and per-task func (`noop` / `img_resize`).
- [ ] Add the `noop` guest fn (receive + return) and the single-step `img_resize` guest fn; rebuild.
- [ ] Sanity: N tasks each receive a byte-identical image; for `img_resize`, resized output correct.

### 1b. Roadrunner baseline — stand up the stack

Roadrunner needs (README + `docs/`): **Ubuntu 20.04/22.04 x86_64**, **WasmEdge v0.11.2**,
**containerd ≥1.6 (CRI)**, **crictl ≥1.27**, **Redis ≥6**, and the **containerd-wasmedge shim**
(prebuilt shims in `experiments/evaluation/binaries/wasmedge/`). Workload images are on Docker Hub
(`docker.io/keniack/*`).

- [ ] Provision the WasmEdge + containerd stack on the same cluster (or one node of it).
- [ ] Generate payloads: `experiments/evaluation/input-data/make-payloads.sh`
      (`file_1M … file_500M`), and an **image** payload set for the resize variant.
- [ ] Run the fanout sweep, all three transports:
  - wasmedge baseline: `experiments/evaluation/scripts/intra-inter-node-wasmedge.sh [NUM_TASKS]`
  - runc baseline: `…/intra-inter-node-container.sh`
  - roadrunner modes: `…/roadrunner-{kernel,net,embedded}-mode.sh` (report its best mode)
- [ ] (Secondary) deploy/cold-start microbench: `experiments/motivation/scripts/run_{wasm_,}image_resize.sh`.

#### Reproduction status (2026-06-10) — BLOCKED on this host

Installed the full stack on the dev box via `roadrunner/docs/quick-install.sh` (we have
passwordless sudo): **containerd 2.2.4, Docker 29.5.3, WasmEdge 0.11.2, Redis, runc**, plus all
prebuilt shims (`cwasi`, `cwasim1/2/3`) in `/usr/local/bin` and a `libwasmedge.so.0` ld path +
containerd systemd `LD_LIBRARY_PATH`/`PATH` override. Storage server builds and serves
`file_10M.txt` on `:8888`. Images pull (`keniack/fanout-wasi`, `keniack/alice-lib`).

**The prebuilt Roadrunner shims do not run here** — they are coupled to the authors' original
Ubuntu 20.04 + containerd 1.x environment:
- `cwasim1` (embedded): looks up a **nonexistent snapshot `6294`** (a stale ID baked into the
  prebuilt binary) for the worker module `alice-lib.wasm`; our max snapshot is 21. Building a
  combined single-layer image (both wasm modules) did not help — the lookup ignores the live
  container snapshot.
- `cwasim3` (net): **`Bind guest directory failed:54`** — the runtime cannot set up its guest
  working dir on containerd 2.x, so the staged file never reaches the driver (`args[3]` panic).

Root cause: prebuilt binaries built against containerd 1.x snapshot/mount APIs. Fixing means either
(A) **rebuild the shim from `roadrunner/app/` source** ported to current containerd libs (uncertain;
`containerd-shim 0.3.0` → 2.x), or (B) run on a **matching Ubuntu 20.04/22.04 + containerd 1.6**
node (their supported config).

**Deeper investigation (2026-06-10) — pushed past two of three blockers, hit a hard wall:**
1. The snapshot `6294` is **literally hardcoded in the shim binary** (`/var/lib/containerd/
   io.containerd.snapshotter.v1.overlayfs/snapshots/6294/fs/alice-lib.wasm`) — confirmed by it
   appearing even under a **standalone containerd 1.6.36** instance with a *different* root. Worked
   around by placing `alice-lib.wasm` at that exact path → the shim then loads the worker module.
2. After that, the driver runs but fails at **`Bind guest directory failed:54`** — WasmEdge 0.11.2's
   WASI dir-preopen is **incompatible with this host's kernel** (same root cause as plain
   `wasmedge --dir` failing, errno 44). Without the dir bind, the runtime can't stage the fetched
   file as `argv[3]`, so the driver panics.
3. Swapping in **WasmEdge 0.13.5**'s `libwasmedge.so.0` (kernel-compatible dir binding) **crashes the
   shim at startup** (`bitset::reset out_of_range`) — the prebuilt shim is ABI-locked to WasmEdge
   0.11.2.

Net: the prebuilt shim needs WasmEdge 0.11.2's exact ABI, but 0.11.2 can't bind guest dirs on this
kernel. **No config workaround exists.** Self-running the transport here requires rebuilding the
shim from `roadrunner/app/` source against a newer WasmEdge (port `wasmedge-sdk 0.7.1`/`containerd-
shim 0.3.0` → current) — real, uncertain effort — or option (B), the matching-OS node.
Stack left installed (containerd 2.2.4 + WasmEdge 0.11.2 lib) for any retry.

**Interim:** use Roadrunner's **published intra-node numbers**
(`experiments/evaluation/results/parallel/intra-node/*.csv`, columns `RoadRunner_Embedded,
RoadRunner, RunC, Wasmedge`) for the comparison — complete, and exactly what their paper reports.
`Tests/ImageResize_Fanout/plot.py` overlays them.

### Dataset (standardize across both systems)

- **Image payloads** sized like their text payloads (1 MB → 500 MB) so delivery cost is comparable.
  Our `TestData/img_*.ppm` are tiny demos — generate a sized PPM/PNG set (a `gen_images` helper in
  the Tests folder). Use the **same image bytes** as Roadrunner's `func_b` input.
- [ ] Pick a **fixed image-size sweep** (e.g. 1 / 10 / 50 / 100 MB) used on both sides.

### Run matrix for Image-resize / fanout

| Axis | Values |
|------|--------|
| System | Ours (SHM / RDMA), Roadrunner (Embedded intra-only + best mode), Roadrunner-wasmedge (WASI), Roadrunner-runc |
| Per-task func | **noop** (pure transport, primary), img_resize (transport + 1-step compute) |
| Fanout degree N | 1, 10, 20, 40, 60, 80, 100 (match their `NUM_TASKS` sweep) |
| Payload (image) size | fixed 10 MB for the fanout sweep (their default `file_10M`); 1/10/50/100 MB for the size sweep |
| Topology | **intra-node** (ours: SHM page-chain) **and inter-node** (ours: 2-node RDMA) — both, like Roadrunner |

### Metrics to capture

Total throughput (MB/s delivered, items/s), end-to-end + per-task latency (median, p95/p99),
**serialization throughput/latency** (ours = 0), peak/billable memory (RAM), CPU (user/kernel/total),
RDMA bytes (ours, inter-node). Mirror Roadrunner's CSV columns so curves overlay directly.

### Definition of done (Image-resize / fanout)

- [ ] Our system: {noop, img_resize} × fanout sweep × {intra-node SHM, inter-node RDMA}, output
      validated, metrics logged to `Tests/ImageResize_Fanout/results.csv`. (noop is the primary,
      pure-transport number.)
- [ ] Roadrunner: matching fanout sweep across {Embedded (intra only), roadrunner, wasmedge, runc},
      both topologies, metrics collected.
- [ ] One results table + the headline plot (throughput/latency vs fanout, **zero serialization**
      annotated, intra vs inter), committed under `Tests/ImageResize_Fanout/figs/` and referenced
      from `Benchmarks/`.

---

## Backlog — remaining workloads (detail when we get there)

Order follows `workload_selection_table.tex`; each gets its own section like Workload 1 above when it
becomes the focus.

2. **WordCount** — RMMap / Faasm / Cloudburst. Native on our system
   (`Executor/guest/src/workloads/word_count.rs`; DAGs under `DAGs/{workload,symbolic,cluster,rdma_workload}_dag/`).
   Primary baseline RMMap (native, our closest twin on ser/deser-free transfer); Faasm (WASM
   map→reduce Faaslet port) and Cloudburst (`map→reduce` DAG over Redis) as ports. Standard corpus:
   `TestData/corpus_large.txt` (50 MB) / `corpus_xlarge.txt` (500 MB) / `corpus.txt` (1 GB),
   replicated to each baseline. Existing harness: `Tests/fan_out_test/`, `Tests/RDMA-InputScale/`.
3. **FINRA** — RMMap / Faasm / Cloudburst. Native (`finra.rs`); multi-stage stateful DAG. Primary
   baseline RMMap `finra/`. **Stretch ports** (after the RMMap head-to-head is solid, against a
   frozen shared spec — same 8 audit rules, same `trades.csv`, same reference data): Faasm (chained
   Faaslets), Cloudburst (function-composition over Redis, reusing the `predserving` harness shape).
4. **Matrix multiplication** — RMMap / Faasm / Cloudburst. Build on our system (fan-out + aggregate
   routing). Native baselines: **Faasm `matrix-multiply`** (divide-and-conquer recursive chaining)
   and **Cloudburst `summa`** (2D block). **Port:** RMMap (DAG of submatrix multiplies + merge;
   showcases its serialization-free RDMA block transfer — moderate effort).
5. **ML training** — RMMap / Faasm / Cloudburst. Native (`ml_training.rs`). Primary baseline RMMap
   `ml-pipeline` (closest twin: RDMA, ser-free state transfer); Faasm HOGWILD! SGD secondary;
   **stretch port** Cloudburst (distributed SGD over Redis via `centr_avg`/`dist_avg`; heavier,
   freeze a shared spec first).
6. **ML inference (MNIST)** — RMMap / Cloudburst / Faasm. **Standardize on MNIST** across all three
   (chosen over MobileNet: lightest to deploy, uniform model for an apples-to-apples comparison).
   RMMap ships MNIST natively (`digital-minist`); Cloudburst and Faasm otherwise default to MobileNet
   — port them to MNIST.
7. **MediaReview** — RTSFaaS. Re-implement as a keyed stateful event stream on our stream-processing
   model (see `WORKLOAD_SELECTION.md` § "MediaReview on our stream-processing model"). Heaviest
   baseline infra (TiKV + lease CC + Java); we do **not** reproduce the stack — we compare the
   *application* on throughput/latency and state-transfer cost.

---

## Open questions (need answers to finalize Phase 0 + Workload 1)

1. **Cluster:** how many nodes, and is the RDMA testbed already up?
2. **Roadrunner stack:** is a node with Ubuntu 20.04/22.04 + WasmEdge v0.11.2 + containerd shim
   available, or is standing it up part of this task?
3. **Resize vs pure transport:** *resolved* — do **both**, `noop` (empty per-task fn = pure
   transport, primary, mirrors Roadrunner's receive-and-return `func_b`) first, then `img_resize`
   (single-step) as the "transport + compute" data point.
4. **Image payload set:** OK to generate a sized PPM/PNG sweep (1/10/50/100 MB) and use identical
   bytes on both systems?
