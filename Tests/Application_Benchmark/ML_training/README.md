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
> binary blob (label `i32` + features `i8`); the per-epoch gradient workers then do a flat
> byte→int read with no per-sample `String`/`Vec` allocation — the same advantage the
> baselines get for free by unpickling a binary numpy array. This ~**doubled** our throughput
> at every size (e.g. 600k/W=16: 5.0 s → 2.66 s) with the checksum gate unchanged. The `i8`
> feature packing also keeps total SHM under the 64 MiB initial pool at 600k (the `i32` form
> overflowed it and exposed a latent SHM-growth bug — see `problems.md`).

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
| 100k    | 0.66   | 1.03      | **0.60** | 1.33       |
| 300k    | **1.43** | 3.64    | 3.41     | 3.99       |
| 600k    | **2.66** | 7.92    | 6.67     | 8.56       |

Peak **throughput** (M sample-gradients/s, max over W):

| dataset | WasMem | Faasm-like | RMMap-ES | Cloudburst |
|---------|--------|-----------|----------|------------|
| 100k    | 1.51   | 0.97      | **1.65** | 0.75       |
| 300k    | **2.10** | 0.82    | 0.88     | 0.75       |
| 600k    | **2.26** | 0.76    | 0.90     | 0.70       |

State **serialized through the KVS** per run (the cost our SHM avoids) — **WasMem = 0**;
baselines re-serialize the data + model + gradients every epoch:

| dataset | WasMem | Faasm | RMMap | Cloudburst |
|---------|--------|-------|-------|------------|
| 100k    | **0**  | 130 MB | 143 MB | 143 MB |
| 300k    | **0**  | 390 MB | 429 MB | 429 MB |
| 600k    | **0**  | 779 MB | 857 MB | 857 MB |

> **The result.** With the kernel held identical across all four systems, WebAsShared's
> zero-copy substrate **wins decisively at 300k and 600k** — ~**2.4–2.5× faster than the next
> baseline (RMMap-ES)** on latency, and the margin **grows with dataset size**: the baselines
> re-serialize the data + model + gradients through Redis *every epoch* (130 MB → **857 MB** by
> 600k), exactly the transfer our SHM page-chain reports as **0**. WasMem also holds the
> **lowest peak memory** at 600k (249 MB vs 356–435 MB) — the data sits once in SHM instead of
> being duplicated through Redis + pickle buffers.
>
> At the **smallest size (100k) WasMem essentially ties RMMap-ES** (0.66 s vs 0.60 s) and beats
> Faasm (1.03 s) and Cloudburst (1.33 s). The residual gap to RMMap is honest: our DAG spawns a
> *fresh OS process per gradient worker per epoch* (E·W ≈ 160 spawns even with AOT — AOT removes
> the JIT, not the process+instantiate cost), whereas RMMap reuses a persistent worker pool, and
> at 100k the ~140 MB of Redis traffic it pays is still cheap relative to that fixed spawn cost.
> So the substrate advantage **starts neutral when the serialized volume is small and dominates
> once it is large** — the mirror image of Matrix's nuance (there the zero-copy win *shrank* as
> O(N³) compute amortized the transfer; here it *grows* as the per-epoch serialized state
> outpaces a fixed spawn overhead). The WasMem curve flattens past W≈8 at 100k (spawn overhead
> ≈ parallelism gain), whereas at 600k more workers keep helping.
>
> *(Earlier, before the `sgd_encode` parse-once optimization, WasMem re-parsed each text shard
> every epoch and 100k was a clear loss at 1.11 s; hoisting the parse out of the E-loop ~halved
> our times and turned 100k into a tie and 300k/600k into ~2.5× wins.)*

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
