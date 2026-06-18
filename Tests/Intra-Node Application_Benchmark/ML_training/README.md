# ML training (SGD) — WebAsShared vs RMMap / Faasm / Cloudburst

Workload **#5**. Distributed model training by **synchronous data-parallel SGD**, the
distributed-training workload that stresses **per-iteration state transfer** (the model
broadcast out + the gradients gathered back, every epoch). RMMap's `ml-pipeline` is the
primary baseline (our closest twin: RDMA, serialization-free state), with Faasm (HOGWILD!
shared-state SGD) and Cloudburst as secondary baselines. Built following
[`../EXPERIMENT_RUNBOOK.md`](../EXPERIMENT_RUNBOOK.md) §9.

```
load(slot 0) → sgd_partition(W) → shards[10..10+W]                  (zero-copy split)
  per epoch e:  sgd_grad ×W  ─┐
       (read shard + latest    ├─ Aggregate → sgd_update (1 central step, model→slot 1900)
        broadcast model)      ─┘
  … ×E epochs …  → sgd_validate → save
```

## The model + why the gate is exact and fan-out-invariant

A **multi-class linear classifier** (one integer weight row per class) trained by full-batch
gradient descent on a **least-squares one-hot objective** — *no transcendental functions*, so
it reproduces **to the bit** in Rust-WASM and in numpy. Frozen spec (identical for every
system): `N_CLASSES=10`, `SGD_TARGET=1024`, `SGD_LR_K=16`, `lr_den = n_samples·LR_K`, weights
start at 0, `E=10` epochs.

Two integer-arithmetic facts make the runbook-style correctness gate hold (§1/§5.2):

1. **Fan-out invariance.** The per-epoch gradient is a *sum* of per-sample contributions.
   Integer addition is associative+commutative, so Σ over the whole dataset is identical no
   matter how the samples are sharded across workers → the trained model is **the same at
   every W**.
2. **Cross-system exactness.** The one learning-rate division is applied **once, centrally**,
   to the aggregated gradient sum (never per-worker), with **toward-zero truncation** that the
   Python baselines replicate. All arithmetic is `i64` → no float drift between WASM and numpy.

**Gate = `weight_checksum`** (Σ of all model weights after E epochs). It MUST be identical
across every system and every W for a given dataset; the harnesses flag any mismatch.

| dataset | checksum | accuracy |
|---------|----------|----------|
| 100k    | **831**  | ~100%    |
| 300k    | **841**  | ~100%    |
| 600k    | **828**  | ~100%    |

**All four systems reproduce these at every W** (validated below). Accuracy climbs with epochs
(87.7% @ E=1 → ~100% @ E≥10) so the epoch loop is real work, not a no-op.

> **The native guest was added *additively*** (new `sgd_*` `#[no_mangle]` fns in
> `Executor/guest/src/workloads/ml_training.rs`) per the backward-compat rule — the existing
> PCA/decision-stump stages are untouched. The existing stump pipeline can't anchor a
> cross-system gate (its result depends on the shard count *and* uses f32 PCA that won't
> reproduce against numpy); synchronous integer SGD fixes both and is the "ML training (SGD)"
> the suite actually compares.

## Input

`gen_data.py N OUT [seed]` → a synthetic, learnable **integer** dataset (`label,f0..f15`,
features 0..4, 10 classes, per-class prototypes + bounded integer noise; a deterministic LCG,
no numpy, so the file is identical on any box). Sizes are by **sample count**; `run.sh`
generates `TestData/ml/sgd_<N>.csv` on demand.

## WebAsShared (ours) — native, DONE

`gen_dag.py W E DATA OUT SHM` unrolls the E-epoch DAG (env `ML_WASM` → AOT `.cwasm`):
`partition → sgd_encode ×W (one-time) → [grad ×W → Aggregate → update] ×E → validate`. The
model lives in a **single stream slot** that `sgd_update` appends each epoch; all W workers
**broadcast-read** the latest (reads are non-destructive page-chain traversals, so one slot
fans out to all). The per-epoch gradient SUMS and the model move through the **SHM page chain
with zero serialization** — the headline (`kvs_ser = 0`). `run.sh` sweeps size × W:

```bash
./run.sh "100000 300000 600000" "1 2 4 8 16" 3                              # JIT → results.csv
ML_WASM=Executor/target/wasm32-unknown-unknown/release/guest.cwasm \
  ML_CSV="$PWD/results_aot.csv" ./run.sh "100000 300000 600000" "1 2 4 8 16" 3   # AOT → results_aot.csv
```

`results_aot.csv` is the fair line for the cross-system plot (AOT removes the per-worker guest
JIT).

> **Parse-once optimization (`sgd_encode`).** Because the E epochs are unrolled, naively
> re-reading + re-parsing each worker's TEXT shard *every* epoch was the dominant per-epoch
> cost. A one-time `sgd_encode` stage parses each shard ONCE into a compact little-endian
> binary blob (label `i32` + features `i32`); the per-epoch gradient workers then do a flat
> byte→int read with no per-sample `String`/`Vec` allocation — the same advantage the
> baselines get for free by unpickling a binary numpy array. This ~**doubled** our throughput
> at every size (e.g. 600k/W=16: 5.0 s → 2.85 s) with the checksum gate unchanged. At 600k the
> `i32` form pushes total SHM past the 64 MiB initial pool; the host now re-syncs its SHM
> mapping per wave after a worker grows the file (`dag_runner::sync_mapping_if_grown`), so the
> growth is handled correctly. 

> **Framework note (additive, no edits to existing stages):** a `Func` node passes the guest
> only **one** u32 (`Func`→`WasmVoid` keeps `arg`, drops `arg2`), so `sgd_grad` packs
> `data_slot | (grad_out<<16)` (same idiom as word_count's `base|count<<16`). And the **input
> slot is consumed once** — only `sgd_partition` reads slot 0; `sgd_update` bootstraps
> `n_samples`/`lr_den` from worker-emitted shard counts, so nothing re-reads the (consumed)
> input. Both are framework realities, not benchmark-driven changes.

## Baselines (same frozen spec, same datasets, same gate)

Every system runs the **same integer SGD kernel** (`baseline/sgd_core.py` is a bit-for-bit
numpy port of the guest's arithmetic), so the comparison is purely about the **data substrate**:
WebAsShared moves the per-iteration data/model/gradient state **zero-copy through SHM**, while
the baselines **re-fetch each shard + the model from Redis every epoch** (stateless-function
semantics) and write gradients back — serialized state our SHM avoids. *(SGD's kernel is a
cheap O(N·C·F) linear pass, not an O(N³) GEMM, so it is data-movement-bound — the same-naive-
BLAS-kernel rule of §5.9 doesn't bite; the int64 kernel is identical across all four anyway.)*

| System | Status | Approach |
|--------|--------|----------|
| **RMMap-ES** | ✅ | ml-pipeline as ES (Redis + pickle); each gradient worker a **separate process** (≈ Knative pod) — RMMap's per-function parallelism. No MITOSIS module (ES needs none, §5.7). (`baseline/rmmap/`) |
| **Faasm-like** | ✅ | `sgd_grad.rs` → **wasm32-wasip1**, each gradient computation a fresh **wasmtime** Faaslet; host moves shard/model/gradient through Redis. SAME integer kernel as our guest → true WASM-vs-WASM. (`baseline/faasm/demo/`) |
| **Cloudburst** | ✅ | split→gradient→aggregate over the **Redis runner**, every chunk/model/gradient cloudpickled through Redis, **single process** (gradient fns sequential — its curve is ~flat in W). (`baseline/cloudburst/`) |

```bash
( cd baseline/cloudburst && ./run.sh "100000 300000 600000" "1 2 4 8 16" )   # needs host redis :6379
( cd baseline/rmmap      && ./run.sh "100000 300000 600000" "1 2 4 8 16" )
( cd baseline/faasm/demo && ./run.sh "100000 300000 600000" "1 2 4 8 16" )   # needs rustc +wasm32-wasip1, wasmtime
```

## Results (this box, AOT line for ours, E=10, gate validated everywhere)

Best end-to-end **training latency** (s, min over W) — the paper figure
[`figs/ml_training_bars.pdf`](./figs/ml_training_bars.pdf):

| dataset | WasMem | Faasm-like | RMMap-ES | Cloudburst |
|---------|--------|-----------|----------|------------|
| 100k    | **0.63** | 1.18    | 0.98     | 1.32       |
| 300k    | **1.64** | 3.95    | 3.80     | 4.23       |
| 600k    | **2.85** | 7.85    | 7.43     | 8.33       |

Peak **throughput** (M sample-gradients/s, max over W):

| dataset | WasMem | Faasm-like | RMMap-ES | Cloudburst |
|---------|--------|-----------|----------|------------|
| 100k    | **1.59** | 0.85    | 1.02     | 0.76       |
| 300k    | **1.83** | 0.76    | 0.79     | 0.71       |
| 600k    | **2.11** | 0.76    | 0.81     | 0.72       |

State **serialized through the KVS** per run (the cost our SHM avoids) — **WasMem = 0**;
baselines re-serialize the data + model + gradients every epoch:

| dataset | WasMem | Faasm | RMMap | Cloudburst |
|---------|--------|-------|-------|------------|
| 100k    | **0**  | 130 MB | 143 MB | 143 MB |
| 300k    | **0**  | 390 MB | 429 MB | 429 MB |
| 600k    | **0**  | 779 MB | 857 MB | 857 MB |

### Timing methodology (fairness)

Latency is measured **apples-to-apples**: the timer starts after the raw training file is read
from disk (the only "input staging" excluded for every system) and covers **data distribution
(splitter) + worker startup + the E-epoch loop + gather** — exactly what WasMem's `TOTAL
compute` covers (its `partition`/`encode` waves + a fresh process spawn per gradient worker).
So each baseline's timer includes its splitter Redis-writes **and** its worker startup (RMMap's
`multiprocessing.Pool` of W pods; Faasm's per-shard `wasmtime` spawns; Cloudburst is
single-process — no fork). The Python-interpreter + numpy-import bootstrap (~870 ms, constant
and independent of size/epochs) is excluded for all baselines — the analog of WasMem's
`node-agent` launch, likewise outside `TOTAL compute`. *(An earlier version started the
baselines' timers after pool creation + the splitter, hiding RMMap's ~400 ms pool cold-start;
corrected here — it makes the baselines a little slower, widening WasMem's lead.)*

> **The result.** With the kernel held identical and startup counted consistently, WebAsShared's
> zero-copy substrate **wins at every size on both latency and throughput**, and the margin
> **grows with dataset size** (~**1.5× at 100k → ~2.6× at 600k** vs the next baseline): the
> baselines re-serialize the data + model + gradients through Redis *every epoch* (130 MB →
> **857 MB** by 600k), exactly the transfer our SHM page-chain reports as **0**. WasMem also
> holds the **lowest peak memory** at 600k (331 MB vs 356–435 MB) — data resident once in SHM,
> not duplicated through Redis + pickle.
>
> The win compounds two effects, both favouring the substrate as scale grows: (1) the per-epoch
> serialized volume the baselines pay rises linearly while WasMem pays zero, and (2) WasMem's
> *fresh-process-per-worker* spawn cost (E·W spawns even with AOT — AOT removes the JIT, not the
> process+instantiate) is a fixed overhead that amortizes as the dataset grows. At 100k both
> effects are smallest, so the lead is "only" ~1.5×; by 600k the iterative serialization
> dominates and it's ~2.6×. (Cf. Matrix's converse nuance, where the zero-copy win *shrank* as
> O(N³) compute amortized the transfer.)
>
> *(Earlier, before the `sgd_encode` parse-once optimization, WasMem re-parsed each text shard
> every epoch and 100k was a clear loss at 1.11 s; hoisting the parse out of the E-loop ~halved
> our times — combined with counting baseline startup, 100k is now a clean WasMem win.)*

## Plot

```bash
python3 plot.py    # → figs/ml_training_overlay.pdf, figs/ml_training_bars.pdf
```

`ml_training_overlay.pdf`: throughput (M sample-grads/s) vs W at 600k + peak throughput per size
bars. `ml_training_bars.pdf` (paper figure): one panel per dataset size (100k/300k/600k), four
systems' best training latency, bars in legend order Cloudburst→RMMap→Faasm→WasMem, StateSync
palette, PDF only.

## Honest scope

- Cloudburst on **Redis, not Anna**, single-process (no per-function parallelism) — don't
  compare to its Anna-locality numbers (§4.4).
- RMMap **ES** protocol only (no MITOSIS kernel module / DMERGE RDMA, §5.7); the per-iteration
  serialization is the inherent ES cost DMERGE is built to beat.
- Faasm-like abstracts the Faaslet mechanism (fresh wasmtime instance + Redis state); no Faasm
  scheduler / snapshots. The **integer kernel is identical** to our guest, so it is a true
  WASM-vs-WASM data-path comparison.
- Memory metrics use each system's own definition (ours = Σ host-tree RSS + SHM once; baselines
  = process peak RSS) — directional, not identical definitions.
