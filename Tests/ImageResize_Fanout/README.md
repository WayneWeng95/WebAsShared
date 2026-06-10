# Image-resize / fanout — WebAsShared vs Roadrunner

Workload **#1** in [`../../Benchmarks/workload_selection_table.tex`](../../Benchmarks/workload_selection_table.tex).
Validates our headline claim: **zero-copy, serialization-free data delivery to WASM functions**.

We compare **one-to-many image delivery** (one image fanned out to N resize functions) on:

| System | Transport | How |
|--------|-----------|-----|
| **Ours (intra-node)** | shared-memory page-chain (zero-copy) | `node-agent run` a fan-out DAG |
| **Ours (inter-node)** | RDMA, serialization-free | `node-agent submit` a 2-node fan-out DAG |
| **Roadrunner** | `splice`/`vmsplice` (best mode) | `compare_system/roadrunner/.../roadrunner-*-mode.sh` |
| **Roadrunner-wasmedge** | vanilla WASI (copies) | `.../intra-inter-node-wasmedge.sh` |
| **Roadrunner-runc** | container | `.../intra-inter-node-container.sh` |

The Roadrunner side is **not run by this harness** — it needs its own WasmEdge + containerd stack
(see `../../Benchmarks/TEST_PLAN.md` § Workload 1b). This folder drives **our** side and collects its
numbers into `results.csv` in the **same column shape** as Roadrunner's
`experiments/evaluation/results/parallel/inter-node/inter-node-fanout-*.csv`, so the curves overlay.

## Layout

```
ImageResize_Fanout/
├── README.md          # this file
├── gen_images.py      # generate sized payloads (our internal binary image format)
├── gen_dag.py         # build a one-to-many fanout DAG for a given fanout degree N
├── run.sh             # sweep fanout × image-size on OUR system, write results.csv
├── plot.py            # delivery/throughput vs fanout, ours vs Roadrunner CSVs
├── results.csv        # our results (generated)
└── figs/              # plots (generated)
```

## How our equivalent maps to the workload

`gen_dag.py` emits this DAG (one-to-many, zero-copy delivery):

```
load (Input, binary)            image bytes -> I/O slot 10
  -> ingest (StreamPipeline)    img_ingest: I/O 10 -> stream slot 20       (staging)
  -> fanout (Shuffle/Broadcast) splice stream 20 -> N slots 100..100+N-1   (the delivery)
  -> w0..w{N-1} (WasmVoid)      img_noop | img_resize on each slot          (the worker wave)
```

- **`Shuffle{Broadcast}` is the delivery.** It splices the source page chain into all N downstream
  slots *without copying the bytes* (`chain_onto` points each slot head at the source's pages) — the
  zero-copy claim. There is intentionally **no `FreeSlots`** node: all N slots share one physical
  chain, so freeing more than one would double-free; a fresh SHM file per run handles cleanup instead
  (`run.sh` removes it before each rep). This matches the existing convention
  (`DAGs/demo_dag/dag_demo.json` broadcasts and frees nothing).
- **Per-task func** (`FUNC=`): `img_noop` (receive + return = pure transport, mirrors Roadrunner's
  `func_b`) or `img_resize` (one downscale step). Both are single-arg `WasmVoid` workers run as one
  parallel wave.

Guest functions are in `Executor/guest/src/workloads/img_pipeline.rs` (`img_ingest`, `img_noop`,
`img_resize`) — additive, no framework code changed.

## Prerequisites

- Built system: from the project root, `./build.sh` (produces `node-agent`, `host`, guest WASM).
  Rebuild after editing the guest functions.
- Image payloads generated (see Run).
- Inter-node (`TOPO=inter`): **not wired yet** — needs a 2-node RDMA fanout DAG generator + a running
  cluster (TEST_PLAN.md § 1a). `run.sh` errors out with a pointer if asked.

## Run (our system)

```bash
# from project root
python3 Tests/ImageResize_Fanout/gen_images.py            # sized payloads -> TestData/fanout_img/*.img
Tests/ImageResize_Fanout/run.sh                           # default: 10MB, fanout 1..100, noop
Tests/ImageResize_Fanout/run.sh "10" "1 10 20 40 60 80 100"   # sizes(MB), fanouts
FUNC=resize Tests/ImageResize_Fanout/run.sh               # transport + 1 resize step
python3 Tests/ImageResize_Fanout/plot.py                  # figs/
```

## Results columns (`results.csv`)

`size_mb, fanout, topo, func, compute_ms, delivery_ms, worker_ms, throughput_mbps, reps`
(medians over `reps`, parsed from `[DAG][timing]`):

- **`delivery_ms`** — the broadcast/splice wave. The **headline**: ~flat as fanout grows (zero-copy →
  delivering to N workers costs about the same as to 1).
- **`worker_ms`** — the N-worker parallel wave (wasmtime subprocess spawn + JIT + per-task compute;
  comparable to Roadrunner spawning N tasks).
- **`compute_ms`** — total (ingest + delivery + workers); the runner excludes the staging file-load.

Early single-node noop numbers (1 MB): `delivery_ms` ≈ 0.02–0.04 ms across fanout 1→16 while
`worker_ms` scales 69→782 ms — the *delivery* is effectively free and constant; the cost is worker
cold-start, exactly the axis Roadrunner's transports differ on.

Roadrunner runs the fanout on **both** intra-node and inter-node (same scripts, switched by
`STORAGE_IP`/`FUNCB_URL` env), so we sweep both topologies too. Its `func_b` just receives and
returns — so the **`noop` (empty per-task fn) variant is the apples-to-apples transport baseline**;
`resize` adds one compute step on top.

## Run matrix

| Axis | Values |
|------|--------|
| Per-task func (`FUNC=`) | **noop** (pure transport, default), resize (1-step) |
| Topology (`TOPO=`) | **intra** (SHM, default), **inter** (RDMA 2-node) |
| Fanout N | 1, 10, 20, 40, 60, 80, 100 (match Roadrunner `NUM_TASKS`) |
| Image size | 10 MB fixed for the fanout sweep; 1/10/50/100 MB for the size sweep |

## Metrics

delivery time (zero-copy splice) and total throughput, worker-wave time, end-to-end compute;
**serialization cost (ours = 0)**. Memory / CPU / RDMA-bytes still to wire in from
`NodeAgent/agent/src/metrics.rs` (`/tmp/node_agent_metrics.jsonl`).

## Status

- [x] `img_noop` + `img_resize` guest fns and the `gen_dag.py` fanout DAG authored & building
- [x] Our intra-node sweep runs end-to-end (delivery/worker/total captured)
- [ ] Wire in per-run memory/CPU/RDMA metrics from the node-agent metrics log
- [ ] Inter-node (RDMA 2-node) DAG generator + sweep
- [ ] Roadrunner sweep collected (separate stack)
- [ ] Overlay plot + results table committed
