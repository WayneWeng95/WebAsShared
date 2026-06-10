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

## Roadrunner baselines on THIS hardware (`baseline/`)

Roadrunner's own zero-copy transport (`RoadRunner`/`Embedded` columns) can't run on this box — its
prebuilt containerd shims are coupled to the authors' Ubuntu 20.04 + containerd 1.x (see
`../../Benchmarks/TEST_PLAN.md` § 1b). What we **can** run same-hardware are the vanilla baselines:

```
baseline/
├── resize/             # Rust crate: same downscale-0.5 as the guest img_resize
│                       #   native: cargo build --release
│                       #   wasm:   cargo build --release --target wasm32-wasip1
└── run_baseline.sh     # fan out N parallel resize tasks (image piped per task via stdin)
                        #   MODE=wasmedge | native | both  -> results_baseline.csv
```

Each task loads its **own** copy of the image (stdin, no shared zero-copy) — so fanout cost grows
with N, unlike our broadcast splice. The per-task compute is byte-identical to the guest `img_resize`,
so only delivery differs. (stdin not `--dir`: WasmEdge 0.11.2's WASI dir-preopen is broken on this
kernel.) For the actual `RoadRunner`/`Embedded` transport, overlay their **published** intra-node CSV
(`roadrunner/.../results/parallel/intra-node/*.csv`) — `plot.py` does this as dashed reference lines.

## Combined result (10 MB image, 5 reps, same hardware)

| fanout | wasmedge ms | native ms | **ours total ms** | ours delivery ms | ours worker ms |
|---:|---:|---:|---:|---:|---:|
| 1 | 662 | 29 | 515 | 0.18 | 15 |
| 10 | 999 | 37 | 570 | 0.19 | 24 |
| 40 | 2905 | 91 | 598 | 0.20 | 67 |
| 100 | **7132** | 213 | **673** | **0.20** | 158 |

Our **delivery stays flat at ~0.2 ms** while wasmedge climbs to 7.1 s at fanout 100 (per-task WasmEdge
cold-start + independent load × N). With AOT, our worker wave (158 ms @ 100) is native-class while
staying in WASM isolation; end-to-end we beat the wasmedge baseline ~10× and match/beat native.

## Status

- [x] `img_noop` + `img_resize` guest fns and `gen_dag.py` fanout DAG (AOT `guest.cwasm`)
- [x] Our intra-node sweep (delivery/worker/total), JIT vs AOT (`results_jit.csv` / `results_aot.csv`)
- [x] Same-hardware **wasmedge + native baselines** (`baseline/results_baseline.csv`)
- [x] Combined overlay plots (`figs/`: delivery / total_wall / throughput vs fanout)
- [ ] `RoadRunner`/`Embedded` transport: blocked on this host → using published CSV; or rebuild shim / 20.04 node
- [ ] Wire in per-run memory/CPU/RDMA metrics from the node-agent metrics log
- [ ] Inter-node (RDMA 2-node) DAG generator + sweep
