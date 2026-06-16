# ML inference (MNIST) — WebAsShared vs RMMap / Faasm / Cloudburst

Workload **#6**. Distributed **batch inference**: a pre-trained integer linear classifier is
applied to a held-out test set — partition the test data across W workers, each runs the
forward pass (argmax over class scores) on its shard, partials are aggregated, accuracy + a
prediction checksum reported. RMMap's `digital-minist` is the primary baseline (RDMA,
serialization-free state); Cloudburst (mobilenet→MNIST) and Faasm (tflite) are secondary.
Built following [`../EXPERIMENT_RUNBOOK.md`](../EXPERIMENT_RUNBOOK.md) §9.

> **Status: DONE — all four systems swept (size × W, 3 reps), gate validated everywhere.**
> See "Results" for the numbers + figures.

```
load_model(io slot 5) → infer_setup_model ─┐ (broadcast model stream slot)
load_data(slot 0) → infer_partition(W) → shards ─► infer_predict ×W ─┐
                                                                     ├─► Aggregate → infer_reduce → save
                                                    (read shard + model) ┘
```

## The model + why the gate is exact and fan-out-invariant

A **frozen, pre-trained integer linear classifier** (`gen_model.py` → `model,C,F,w…`; the
per-class prototype weights the dataset is built around, so it's an accurate
nearest-prototype classifier — a meaningful "trained" model without a training run).
`C=10`, `F=16`. The forward pass is `score_c = Σ_j w[c][j]·x[j]` (i64) + argmax (first max
wins) — no transcendental functions.

The runbook-style gate holds (§1/§5.2) because:

1. **Fan-out invariance.** Each sample is classified INDEPENDENTLY, so the prediction set —
   and hence `Σ predicted labels` and the correct count — is identical no matter how the test
   set is sharded across W workers. (A single forward pass; no iteration, so this is even
   more direct than the SGD workload's gradient-sum argument.)
2. **Cross-system exactness.** Integer score + argmax → reproducible to the bit in Rust-WASM
   and numpy alike.

**Gate = `prediction_checksum`** (Σ predicted labels over the test set). MUST be identical
across every system and every W.

## Input

Test set = `../ML_training/gen_data.py N OUT seed` with a **held-out seed (9999 ≠ training's
1234)** → `TestData/ml/test_<N>.csv` (same `label,f0..f15` integer format). The model is
`TestData/ml/infer_model.txt` from `gen_model.py`.

## WebAsShared (ours) — native, DONE

`gen_dag.py W MODEL DATA OUT SHM` emits the inference DAG (env `MLI_WASM` → AOT `.cwasm`). The
test shard + the broadcast model move through the **SHM page-chain with zero serialization**
(`kvs_ser = 0`); a single forward pass per sample. Guest fns (additive, new
`Executor/guest/src/workloads/ml_inference.rs`, no edits to existing stages): `infer_setup_model`,
`infer_partition`, `infer_predict`, `infer_reduce`. Sweep:

```bash
./run.sh "100000 300000 600000" "1 2 4 8 16" 3                              # JIT → results.csv
MLI_WASM=Executor/target/wasm32-unknown-unknown/release/guest.cwasm \
  MLI_CSV="$PWD/results_aot.csv" ./run.sh "100000 300000 600000" "1 2 4 8 16" 3   # AOT → results_aot.csv
```

## Baselines (same frozen model + test set + gate)

Every system runs the **same integer forward pass** (`baseline/infer_core.py` is a bit-for-bit
numpy port of the guest kernel), so the comparison is purely about the data substrate:
WebAsShared moves the model + shards zero-copy through SHM; the baselines fetch the model +
each shard from Redis (serialized) and write partial results back.

| System | Status | Approach |
|--------|--------|----------|
| **RMMap-ES** | ✅ scripted | each predict worker a **separate process** (≈ Knative pod); model + shard + result via Redis+pickle; no MITOSIS module (§5.7). (`baseline/rmmap/`) |
| **Faasm-like** | ✅ scripted | `infer_predict.rs` → **wasm32-wasip1**, each shard a fresh **wasmtime** Faaslet; model/shard/result via Redis. SAME integer kernel → true WASM-vs-WASM. (`baseline/faasm/demo/`) |
| **Cloudburst** | ✅ scripted | split→predict→aggregate over the **Redis runner**, cloudpickled through Redis, **single process** (predict fns sequential, ~flat in W). (`baseline/cloudburst/`) |

```bash
( cd baseline/cloudburst && ./run.sh "100000 300000 600000" "1 2 4 8 16" )   # needs host redis :6379
( cd baseline/rmmap      && ./run.sh "100000 300000 600000" "1 2 4 8 16" )
( cd baseline/faasm/demo && ./run.sh "100000 300000 600000" "1 2 4 8 16" )   # needs rustc +wasm32-wasip1, wasmtime
```

## Results (this box, AOT line for ours, gate validated everywhere)

**Correctness gate — all four systems, every W:** `prediction_checksum` is a single value per
test set (100k → **308999**, 300k → **930242**, 600k → **1861022**), accuracy ~**74%**
(74.19 / 74.25 / 74.27). A realistic inference number (prototype linear model on noisy
samples), not a saturated 100%.

Best end-to-end **inference latency** (s, min over W) — the paper figure
[`figs/ml_inference_bars.pdf`](./figs/ml_inference_bars.pdf):

| test set | WasMem | Faasm-like | RMMap-ES | Cloudburst |
|----------|--------|-----------|----------|------------|
| 100k     | **0.077** | 0.160  | 0.443    | 0.106      |
| 300k     | **0.205** | 0.456  | 0.578    | 0.292      |
| 600k     | **0.347** | 0.969  | 0.831    | 0.605      |

Peak **throughput** (M predictions/s, max over W):

| test set | WasMem | Faasm-like | RMMap-ES | Cloudburst |
|----------|--------|-----------|----------|------------|
| 100k     | **1.29** | 0.63    | 0.23     | 0.94       |
| 300k     | **1.46** | 0.66    | 0.52     | 1.03       |
| 600k     | **1.73** | 0.62    | 0.72     | 0.99       |

State **serialized through the KVS** (WasMem = 0) and **peak memory** at W=16:

| test set | kvs_ser: WasMem / Faasm / RMMap / CB | peak MB: WasMem / Faasm / RMMap / CB |
|----------|--------------------------------------|--------------------------------------|
| 100k     | **0** / 13 / 26 / 26                  | **76** / 120 / 100 / 100             |
| 300k     | **0** / 39 / 78 / 78                  | **120** / 242 / 203 / 203            |
| 600k     | **0** / 78 / 156 / 156               | **251** / 439 / 356 / 356            |

### Timing methodology (fairness — read this)

Latency is measured **apples-to-apples with WasMem's `TOTAL compute`**: the timer starts after
the raw test file is read from disk (the only "input staging" excluded for every system) and
covers **data distribution + worker startup + forward pass + gather**. Concretely, WasMem's
`TOTAL compute` already includes its `partition` wave and a *fresh OS process spawn per predict
worker*; so each baseline's timer likewise includes its splitter (the Redis writes that
distribute the shards) **and its worker startup** (RMMap's `multiprocessing.Pool` of W pods;
Faasm's per-shard `wasmtime` spawns — already counted; **Cloudburst is single-process — it
never forks workers, so it has no pod cold-start to add**). An earlier version started the
baselines' timers *after* pool creation + the splitter writes, which hid ~**440 ms** of RMMap
pool cold-start at 100k (its forkserver + per-worker numpy import) and made it look fastest —
corrected here.

The Python-interpreter + numpy-import bootstrap (~**870 ms**, measured constant across workload
and size, since it's pure runtime startup + the file read) is excluded for *all* baselines —
the analog of WasMem's `node-agent` launch, likewise outside `TOTAL compute`. That ~870 ms is
in fact **Cloudburst's only "startup"** (verified: wall − `compute_ms` ≈ 870 ms in both training
and inference, independent of W — i.e. no worker fork), confirming it isn't hiding pod
cold-start the way the pre-fix RMMap timer was.

> **Caveat (cold-start / single batch).** This counts each system's **cold** per-batch startup,
> because WasMem's DAG spawns fresh processes every run and cannot warm-start. A production
> RMMap/Knative deployment keeps its worker pods **warm** across many requests, amortizing that
> ~440 ms — so in steady-state serving RMMap's per-request latency would be far lower than the
> single-batch number here. The honest reading is: **for cold/one-shot batch inference WasMem
> wins on all of latency, throughput, serialization (0) and memory**; for warm high-QPS serving
> the startup term vanishes and the comparison would reopen.

> **The result.** With startup counted consistently, **WasMem leads at every size** on latency
> and throughput, serializes **zero** bytes (vs 13–156 MB), and holds the **lowest peak memory**.
> But the margin here is *not* the substrate story it is for training: inference is a **single
> forward pass**, so the KVS is crossed only ONCE (13–156 MB, vs ML-training's 857 MB) and the
> baselines' real handicap is **per-batch worker/pod cold-start + one round of Redis
> distribution**, not iterative serialization. This is the principled complement of ML-training:
> there the zero-copy substrate dominated because state moved *every epoch*; here, with one-shot
> movement, WasMem's win comes mostly from **lightweight in-SHM workers (no pod cold-start) +
> zero-copy distribution**. RMMap-ES is slowest at 100k (cold pool dominates) and recovers
> relatively as the batch grows and startup amortizes.

## Plot

```bash
python3 plot.py    # → figs/ml_inference_overlay.pdf, figs/ml_inference_bars.pdf
```

Same conventions as the other workloads (StateSync palette; bars = best inference latency per
size, legend order Cloudburst→RMMap→Faasm→WasMem; PDF only).

## Honest scope

Same as the other workloads: Cloudburst on Redis (not Anna), single-process; RMMap ES only
(no DMERGE/MITOSIS); Faasm-like abstracts the Faaslet mechanism (fresh wasmtime + Redis) with
the **identical integer kernel** as our guest. Memory metrics use each system's own definition.
