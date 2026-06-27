# Inter-node bring-up & baselines — session progress (2026-06-26)

Running log of what was done bringing the 4-node cluster online and adding the k8s
baselines. Newest work at the bottom.

## Status snapshot — WordCount 4 GB head-to-head (all inter-node baselines)

| system | makespan | total_job | occ | how |
|--------|---------:|----------:|-----|-----|
| **WasMem** (ours) | 13.8 s | 52 s | 715.2 M | RDMA page-chain, zero-copy |
| Faasm | 15.7 s | 550 s | 715.2 M | per-node Faaslets + Redis KV |
| **RMMap** | **16.2 s** | 850 s | 660.8 M | user-space one-sided RDMA read (no kmod) |
| Cloudburst | 37.1 s | 682 s | 660.8 M | k8s executor pool over a Redis work queue |

RMMap is the **RDMA** variant — the ES (Knative+Redis) fallback is **dropped** (§5).
`occ` differs (715.2 M vs 660.8 M) only because Cloudburst/RMMap ran on the **regenerated**
corpus (recovered 12 KB seed tiled to 4000 MiB) — same size, self-consistent gate per
system. The figure (`analysis/figs/inter_node_bars.{pdf,png}`, copied to `Tests/Figures/`)
plots one bar per system in order **Cloudburst, RMMap, Faasm, WasMem**; RMMap = the RDMA row.
**✅ ALL 6 WORKLOADS DONE for all four baselines** — TeraSort (§7), FINRA (§8), Matrix (§9),
ML-training + ML-inference (§10), alongside WordCount. The only remaining work is the
inter-node memory-cost tracking (§ Open tasks). Detail per section below.

## 1. Kubernetes cluster — UP (4/4 nodes)
- k3s was already a server on node-0; the **3 workers had never joined**. Joined
  node-1/2/3 via `02-install-agent.sh` (SSH from node-0 now works) → **4/4 Ready**.
- Added the missing pieces: **Strimzi + Kafka** (10 topics), **in-cluster Redis**
  (`baselines` ns, NodePort `10.10.1.2:30679`). Knative+Kourier already present.
- `04-verify.sh` → `CLUSTER READY ✓` (cross-node CNI + Knative scale-from-zero OK).

## 2. Flink StateFun streaming benchmark — DONE
- Distributed the 3 `streaming-example-*` images to every node's containerd; applied
  master/worker + mediareview/socialnetwork function manifests (`imagePullPolicy:
  IfNotPresent`).
- Ran **24×1 (parallelism 24)** and **3×16 (parallelism 48)** configs. Both gates
  PASS exactly; throughput in
  `Streaming Application_Benchmark/inter-node/baseline/FlinkStateFun/results_throughput_statefun{,_scaled}.csv`.
- **Gotcha (saved to memory `statefun-cluster-scaling`):** 48×1 / 24×1 flapped on
  JobManager heartbeat timeout during deploy. Fix = JM heap 3g + `heartbeat.timeout
  200s` + `akka.ask.timeout 120s`, and **restart BOTH master AND workers** so TMs
  pick up the timeout. Exactly-once egress only commits on checkpoint, so "0
  throughput" = checkpoints not completing.
- Added StateFun to `inter-node/plot.py` and `plot_throughput_combined.py`; figures
  regenerated.

## 3. TestData — DONE (all 6 regenerated; data had been emptied)
Seeds/data were gone. Recovered the WordCount 12 KB seed from **git object `c1479760`**;
all 6 regenerated **on node-0 in `TestData/`** (deterministic seeds → byte-identical input
across systems). Generators live in `Tests/Intra-Node Application_Benchmark/`.

| workload | generator | output file(s) | size | seed → gate |
|---|---|---|---|---|
| WordCount | tile seed `c1479760` | `corpus_4gb.txt` | 4000 MiB | — |
| TeraSort | `TeraSort/gen_records.py 1228.8` | `terasort_1.2gb.txt` | 1.2 GiB (12,884,902 recs) | 1234 |
| FINRA | `Finra/gen_trades.py 5000000` | `finra_5m.csv` | 5M trades, 206 MB | 42 |
| ML training | `ML_training/gen_data.py 6000000` | `ml_training_6m.csv` | 6M, 204 MB | 1234 (C=10 F=16) |
| ML inference | `ML_inference/gen_model.py` + `ML_training/gen_data.py 6000000` | `ml_inference_model.csv` + `ml_inference_6m.csv` | 332 B + 6M (204 MB) | — |
| Matrix | `Matrix/gen_matrix.py 4096` | `matrix_a_4096.bin` + `matrix_b_4096.bin` | 4096² f64, 128 MiB ea | 12345 → checksum **1391095867672** |

## 4. Cloudburst baseline — DONE (WordCount 4 GB, 15 reps)
- Decision: run Cloudburst/RMMap at the **same 4 GB** as wasmem/faasm by **sharding**
  the corpus across many <512 MB Redis keys (chosen over running at a smaller size).
- Built `k8s/cloudburst/` (executor pool over a Redis work queue) — see its README.
  Image on all 4 nodes; 60-pod pool = 15/node. 4 GB run executes; gate consistent.
- **The BLPOP hang — root cause & fix:** the executor only ever signals done on
  *success* and the done signal carried no index, so the driver's single blocking
  `BLPOP`-per-signal hung forever the moment one in-flight task vanished (pod
  evicted/died after BRPOP, before writing a result — leftover Redis had 60 chunks
  but only 56 `_res_` keys, i.e. 4 lost; 0 restarts, no error logged). **Fix:** done
  signal now `"idx:ms"` (busy time keyed by idx, counted once → idempotent under
  re-dispatch); driver replaced the single BLPOP with a **pending-set loop** that
  verifies completion by *result-key presence* and **re-dispatches** any task whose
  result never lands (60 s no-progress timeout, ≤8 rounds). Re-run never stalled.
- **Re-run DONE:** rebuilt+re-imported the executor image to all 4 nodes, 15 reps @
  4 GB → **makespan 37.1 s ± 0.8, total_job 681.8 s, occ 660,848,961** (gate exact
  every rep, no re-dispatch needed). Slowest bar — pays the serial KVS upload + 60
  concurrent 67 MB chunk reads contending on the single Redis.
  `cloudburst/results_wordcount.csv` written; `inter_node_bars.{pdf,png}` regenerated.

## 5. RMMap-ES (Knative + Redis) — built, then **DROPPED** (superseded by §6 RDMA)
RMMap = the **RDMA** variant (§6). The earlier ES fallback (Knative `wc-mapper` Service,
state serialized through Redis — RMMap *without* its mechanism) is **no longer part of the
experiment and won't be re-tested.** It still ran (WordCount 4 GB: warm 41.1 s / 1158 s,
cold 44.0 s / 919 s, occ 660,848,961), and the harness + those `variant=warm/cold` CSV
rows remain in `k8s/knative/rmmap-wordcount/` + `rmmap/results_wordcount.csv` as archived
reference, but they are not plotted, not memory-profiled, and not maintained going forward.

## 6. RMMap **DMERGE / shared-memory** — kmod infeasible, user-space RDMA workaround DONE
Why the ES bar is slow: it is RMMap *without its mechanism* (serialize-through-Redis).
RMMap/DMERGE's real win is RDMA remote-memory read (no serialize). Investigated using it:
- **Real MITOSIS kmod: NOT buildable here.** Repo is on disk (`/opt/myapp/compare_system/
  RMMap`, EuroSys'24 `dmerge` artifact; submodules fetch OK). But `make km` needs Mellanox
  **OFED for kernel 7.0.0-10** (doesn't exist) + pinned **rust nightly-2022-02-04 / clang-9**
  vs this box's stable rustc 1.93 / clang-21. Hard walls — `cp Module.symvers` fails,
  `Cargo.toml` won't parse under cargo 1.93. Parked.
- **Hardware IS there:** Mellanox `mlx4_0` RoCE, **the 10.10.1.x fabric is `eno1d1` =
  mlx4_0 port 2**, links ACTIVE. `ib_uverbs` loaded, `/dev/infiniband/uverbs0`, perftest +
  libibverbs/librdmacm present. Cross-node `ib_read_bw` confirmed (~1.09 GB/s).
- **Built the faithful workaround** (`rmmap-rdma-wordcount/`, no kernel module): user-space
  **one-sided RDMA READ** (`rdma_cm`). node 0 mmaps + `reg_mr`s the corpus **once**; 60
  mapper host-processes (SSH fan-out, 15/node) RDMA-READ their 67 MB chunk straight out of
  that memory, then count with the **same Python `wc_ops`** as the other bars (only the
  *transfer* differs). occ gate exact: 660,848,961.
- **Result (15 reps @ 4 GB):** **makespan 16.2 s ± 0.2, total_job 850 s** (vs RMMap-ES
  41.1 s / 1158 s). RDMA cuts makespan (no per-rep KVS upload — register-once/read-many)
  and total_job (no serialize + no Redis read-contention). Residual 850 s = the **Python
  word-count floor** (same compute as every Python bar), which is why it doesn't reach
  WasMem's 52 s total_job — that gap is compute speed, not transfer.
- Figure: `plot_inter_bars.py` plots the **RDMA** row as the single **RMMap** bar (via the
  `variant` filter; the ES warm/cold rows stay in the CSV but are not plotted). Group +
  legend order is **Cloudburst, RMMap, Faasm, WasMem** (single-row legend). RMMap's makespan
  base (16.2 s) is the shortest of the KVS/RDMA baselines.
- Deployment caveat: RDMA mappers are host processes (RDMA-in-pods needs a device plugin);
  the transfer mechanism compared is unchanged. `cloudburst-executor` restored to 60.

## 7. TeraSort 1.2 GB — DONE (all 4 inter-node baselines, 15 reps)
TeraSort is the **all-to-all shuffle** (the entire dataset crosses the shuffle once — the
maximal-data-movement contrast to WordCount). Ported Cloudburst + RMMap-RDMA; Faasm/WasMem
already had rows. All four run @ 1.2 GB (`TestData/terasort_1.2gb.txt`, 12,884,902 records,
seed 1234), fanout/workers = **4** (the N range owners, matching wasmem/faasm).

| system | makespan | total_job | gate (records, keysum, sorted) | how |
|--------|---------:|----------:|--------------------------------|-----|
| **WasMem** (ours) | **11.2 s** | **28.3 s** | 12,886,999 / 8,312,165,245 / 1 | RDMA page-chain shuffle, zero-copy |
| Faasm | 15.5 s | 51.1 s | 12,886,999 / 8,312,165,245 / 1 | per-node Faaslets + Redis bucket shuffle |
| **RMMap (RDMA)** | **16.8 s** | **57.8 s** | 12,884,902 / 8,310,813,852 / 1 | one-sided RDMA-READ for input **and** shuffle |
| Cloudburst | 24.8 s | 77.6 s | 12,884,902 / 8,310,813,852 / 1 | executor pool, Redis bucket SET/GET shuffle |

Same ordering as WordCount: RMMap-RDMA beats Cloudburst (RDMA shuffle vs Redis bucket
SET/GET), but both share the **Python `ts_ops` compute floor** (partition + sort), so neither
reaches Faasm/WasMem (faster wasm/`ts.cwasm` compute — the residual gap is compute, not
transfer). The `gate` differs between the **Python** bars (Cloudburst/RMMap: 12,884,902 /
8,310,813,852) and the **wasm** bars (Faasm/WasMem: 12,886,999 / 8,312,165,245) exactly like
WordCount's occ split — each system self-consistent across all 15 reps (verified).

- **Cloudburst** (`k8s/cloudburst/`): added `ts_ops.py` + `ts_partition`/`ts_merge` ops in
  `executor.py` + `driver_terasort.py` (two waves: partition scatter → barrier → merge
  gather). **Two scaling fixes** (see its README): (1) raised executor `limits.memory`
  **1Gi → 4Gi** — a fanout-4 partition task loads a 322 MB chunk (peak ~1.3 GB) and OOM-killed
  1Gi pods, the task then vanishing mid-flight; (2) raised `DONE_TIMEOUT` **60 → 300 s** —
  each fanout-4 task legitimately moves ~1/4 of the data through one Redis, so 60 s wrongly
  re-dispatched live tasks. Rebuilt+re-imported the image to all 4 nodes.
- **RMMap-RDMA** (`rmmap-rdma-terasort/`, new): generalized the WordCount RDMA path so the
  **shuffle is also one-sided RDMA-READ**. `rdma_ts.c` adds `serve_idx` (serve a partitioner's
  bucket file by explicit bucket boundaries). Each partitioner RDMA-reads its input chunk from
  node 0, range-partitions, and **publishes** its buckets as a remote-read MR; each merger
  RDMA-reads its owner column out of every partitioner's memory, then sorts — no KVS staging.
  Input corpus published once (register-once/read-many); buckets re-published each rep (the KVS
  bars also re-scatter each rep). Workers run as host processes over SSH (RDMA-in-pods needs a
  device plugin); scale `cloudburst-executor` to 0 during the run, restore to 60 after.
- Figure `analysis/figs/inter_node_bars.{pdf,png}` (copied to `Tests/Figures/`) regenerated —
  the TeraSort group now has all four bars. Finra/Matrix/ML still show only Faasm+WasMem.

## 8. FINRA 5M trades — DONE (all 4 inter-node baselines, 15 reps)
FINRA is a **MAP-only audit fan** (no shuffle): the inter-stage state is just a violation
**count** per worker (like WordCount), but the input is read **~8×** (each of the 8 rule-groups
scans the trades), so it is *input-read-heavy*. Hybrid fan = **3 stateful + 5·S stateless**:
the 5 stateless rules (0,1,5,6,7) are each sharded into S disjoint slices (counts sum exactly);
the 3 stateful rules (2,3,4 = wash/spoofing/concentration) read the full corpus. Fanout 60 →
S=11 → **58 workers**, matching Faasm/WasMem.

| system | makespan | total_job | violations | how |
|--------|---------:|----------:|-----------:|-----|
| **WasMem** (ours) | **10.3 s** | **39.3 s** | 2,271,415 | RDMA shared-mem reads, zero-copy |
| Faasm | 12.8 s | 183.3 s | 2,271,415 | per-node Faaslets + Redis trades upload |
| **RMMap (RDMA)** | **19.3 s** | 204.9 s | 2,271,415 | one-sided RDMA-READ of each worker's trades |
| Cloudburst | 22.7 s | 173.2 s | 2,271,415 | executor pool, Redis trades upload + per-worker GET |

**The gate matches ALL FOUR systems exactly (2,271,415)** — unlike WordCount/TeraSort there is
**no Python-vs-wasm split**: FINRA's Python `finra_rules` is a faithful port of `finra.rs`, so
it computes the identical violation count (verified). RMMap-RDMA **beats Cloudburst on makespan**
(19.3 vs 22.7 s — register-once/read-many avoids Cloudburst's serial trades upload), but both
Python bars carry the **5M-trade parse floor** (`parse_trades` builds 5M dicts, ~2.9 GB,
~30 s for a stateful full-data worker), which is why their `total_job` is far above Faasm/WasMem
(wasm parse). RMMap's `total_job` (205 s) is the tallest — the 58 host-process workers (≈15/node)
contend on CPU for that Python parse, and each stateful worker reassembles all S chunks before
parsing; an honest cost of the host-process deployment, while the **makespan** (the latency the
RDMA transfer actually targets) is the lower of the two Python bars.

- **Cloudburst** (`k8s/cloudburst/`): added `finra_rules.py` (the 8-rule spec + a `skip_header`
  flag) + a `finra_rule` op in `executor.py` + `driver_finra.py` (upload full trades + S
  header-prefixed shards, dispatch 58 `finra_rule` tasks, sum counts — one wave, no shuffle).
  Rebuilt+re-imported the image to all 4 nodes (this time including node-3, which had silently
  lost the TeraSort image → 15 `ImagePullBackOff` pods; re-imported + the stuck pods recovered).
- **RMMap-RDMA** (`rmmap-rdma-finra/`, new): **reuses `rdma_ts` unchanged** (serve S chunks +
  read). node 0 publishes the trades once; each worker RDMA-reads the trades it needs —
  stateless reads its 1/S slice (skip_header only for slice 0), stateful reads all S chunks and
  concatenates the full corpus — then runs the same `finra_rules` audit. Host processes over
  SSH; scale `cloudburst-executor` to 0 during the run, restore to 60 after.
- Figure regenerated — the FINRA group now has all four bars. Matrix/ML still Faasm+WasMem only.

## 9. Matrix 4096² (SUMMA) — DONE (all 4 inter-node baselines)
The first **object-heavy** workload: C = A·B as an R×C block grid, the inter-stage state is
f64 matrix panels/blocks (not flat records). `grid(64)` → **8×8**, blocks 512×512. tile → A
row-panels (BR×N) + B col-panels (N×BC) → block (i,j) computes C_ij = A_i·B_j → assemble folds
the f64 checksum (Σ all entries). Workers = 64, matching Faasm/WasMem.

| system | makespan | total_job | gflops | checksum | how |
|--------|---------:|----------:|-------:|---------:|-----|
| Faasm | 8.1 s | 382.9 s | 16.9 | 1,391,095,867,672 | WASM ikj block, Redis panels + C blocks |
| **WasMem** (ours) | 9.7 s | **34.9 s** | 14.2 | 1,391,095,867,672 | WASM ikj block, RDMA shared-mem panels |
| **RMMap (RDMA)** | **6.5 s** | 312.4 s | 21.1 | 1,391,095,867,672 | one-sided RDMA-READ of A_i+B_j panels |
| Cloudburst | 8.7 s | 124.9 s | 15.9 | 1,391,095,867,672 | Redis panel upload + per-block GET |

**Same algorithm everywhere, same gate (1,391,095,867,672) across ALL FOUR — verified.** The
decomposition is byte-identical (the `grid()` is shared; WasMem's guest `matrix.rs` uses the
same A-row-panel × B-col-panel block). The **kernel** follows the documented fairness rule §5
— a NAIVE non-BLAS kernel per language: WasMem/Faasm use the **WASM ikj** triple loop
(`Executor/guest/src/workloads/matrix.rs`, `…/faasm/demo/matblock.rs`); Cloudburst/RMMap use
**`np.einsum('ik,kj->ij', optimize=False)`** (numpy's nested-loop contraction, NOT BLAS —
`optimize=False` is what keeps it off GEMM), exactly as the intra-node Matrix baseline
established. The matrix entries are integers < 2^53, so the checksum is exact and
order-independent → identical across the ikj and einsum kernels. (We briefly considered a
pure-Python no-numpy kernel for Cloudburst/RMMap, but it is ~80 s/block / ~830 s/rep — it just
slows the bars without changing the substrate being compared, so we kept the einsum kernel.)

WasMem's **total execution (34.9 s)** dwarfs every baseline — the data-substrate win (its
panels move zero-copy via the RDMA page-chain; the rest serialize through Redis or pay
host-process compute). RMMap-RDMA has the **lowest makespan** (6.5 s — panels published once,
read-many, no per-rep upload) but a high total_job (312 s): its 64 host-process einsum blocks
(16/node on 16 cores) contend, an honest cost of the host-process deployment, while the
makespan (the latency RDMA targets) is the shortest of the four.

- **Cloudburst** (`k8s/cloudburst/`): added a `mat_block` op (numpy einsum) to `executor.py` +
  `driver_matrix.py` (tile → 64 block tasks → assemble checksum). The image now also carries
  **numpy** (`Dockerfile`); rebuilt + re-imported to all 4 nodes.
- **RMMap-RDMA** (`rmmap-rdma-matrix/`, new): reuses `rdma_ts serve_idx` to publish ONE buffer
  holding all panels (A row-panels contiguous as-is + B col-panels repacked contiguous); each
  block RDMA-reads chunk[i]=A_i and chunk[R+j]=B_j, einsums, reports its partial checksum
  (shipping the 2 MB C blocks — small vs the 2 GB panel broadcast — is omitted like the
  WordCount mappers' counts). Panels published once; nodes need numpy.
- Figure regenerated — the Matrix group now has all four bars. ML-training/inference remain.
- **node-3 image-GC gotcha recurred** (scale-0 → GC → re-import after scale-60) — saved to memory.

## 10. ML training + ML inference 6M — DONE (all 4 baselines) → ALL 6 WORKLOADS COMPLETE
Both ML workloads are **MAP-only** (like FINRA): inter-stage state is tiny (a [C,F]=10×16
gradient, or a (correct,total,predsum) triple); the heavy cost is the per-worker CSV parse of
6M samples. Both **reuse the shared integer kernels** `sgd_core` / `infer_core` (the same
modules the wasm guests mirror), so the gates are exact. Fanout 60, 4 nodes, 15 reps.

**ML training (6M, one-epoch SGD):** WasMem 5.3 s / **16.4 s** total · Faasm 4.7 / 49.8 ·
**RMMap-RDMA 3.3 / 120.1** · Cloudburst 5.5 / 139.6. acc ~74.3%. **weight_checksum 1232 for
ALL FOUR** (deterministic integer SGD).

**ML inference (6M):** WasMem 3.5 s / **12.3 s** · Faasm 3.1 / 46.5 · **RMMap-RDMA 2.0 / 47.4**
· Cloudburst 2.5 / 74.3. **prediction_checksum SPLITS**: Cloudburst/RMMap use the
**regenerated** `ml_inference_model.csv` → **18,633,154**; Faasm/WasMem used the original
`infer_model.txt` (now gone) → 18,623,474 — same regenerated-data split as WordCount, each
self-consistent across all 15 reps.

- RMMap-RDMA has the lowest makespan on both (samples published once → no per-rep upload);
  total_job follows the usual pattern (Python parse floor high on Cloudburst/RMMap, wasm parse
  far lower on WasMem — the substrate win).
- **Cloudburst** (`k8s/cloudburst/`): copied `sgd_core.py`+`infer_core.py` into the image,
  added `ml_train`+`ml_infer` ops to `executor.py`, `driver_ml_{training,inference}.py` (one
  map-reduce wave). Rebuilt + re-imported.
- **RMMap-RDMA** (`rmmap-rdma-ml/`, new): reuses `rdma_ts serve`+`read`; one `ml_worker.py`
  (`--mode train|infer`) + one `driver.py --mode` (gradients returned base64; model scp'd to
  nodes for infer).
- Figure regenerated — **all 6 workload groups now have all 4 bars.**

## ✅ ALL 6 INTER-NODE WORKLOADS DONE for all 4 baselines (Cloudburst, RMMap-RDMA, Faasm, WasMem)
WordCount (§4/§6), TeraSort (§7), FINRA (§8), Matrix (§9), ML-training + ML-inference (§10).
The Cloudburst executor image carries every stage op (wc/ts/finra/matrix/ml) + numpy; the
RMMap-RDMA harnesses (`rmmap-rdma-{wordcount,terasort,finra,matrix,ml}/`) all use the same
`rdma_ts` user-space one-sided RDMA-READ transport.

## Open tasks (remaining / future)
- ~~Port the 6 workloads to Cloudburst + RMMap~~ ✅ **DONE (§4,6,7,8,9,10).**
- **Track total memory cost (like the intra-node case) — NOT YET DONE.** The current
  inter-node harnesses only record time (`makespan`/`total_job`); they do **not** capture
  memory. The intra-node suite reports `peak_mem_mb` = high-water **Σ RSS of the whole
  process tree** (`resource.getrusage` SELF+CHILDREN `ru_maxrss`) and a **total = peak_mem_mb
  + external-KV resident bytes** (`kvs_ser_mb`/`state_kv_mb`; read traffic excluded; WasMem
  has none), aggregated by `Intra-Node Application_Benchmark/analysis/mem_footprint.py` →
  `mem_footprint.md` / `mem_largest_load.pdf`. Replicate that inter-node, adapting Σ RSS to
  the distributed run:
  - **Cloudburst** (pods): peak working-set per pod across all 4 nodes via cAdvisor /
    `kubectl top pods` / `container_memory_working_set_bytes`, summed over the 60 executor
    pods + driver; **+ Redis resident bytes** (the sharded ~4 GB corpus + partials) as the
    external-KV term.
  - **RMMap-RDMA**: Σ peak RSS of the 60 mapper host-processes across nodes (`ru_maxrss`,
    collected back over SSH) + the driver; **+ the registered MR** (the 4 GB corpus pinned
    in node-0 RAM) as its "external store" analogue — no Redis.
  - **Faasm** (track-D per-node agent): Σ peak RSS of the Faaslet process tree on each node
    (`ru_maxrss`, summed across nodes) **+ Faasm's Redis KV resident bytes** (its state
    store). Same whole-tree accounting the intra-node Faasm `peak_mem_mb` already uses.
  - **WasMem** (ours): Σ peak RSS across the node-agent coordinator + workers on all 4
    nodes (whole host tree per node, summed) — the RDMA page-chain shared-memory substrate
    is WasMem's state and is **already in host RSS** (pinned pages), so **no external-KV
    term** (total = Σ peak RSS), matching the intra-node treatment.
  - The existing `wasmem/` + `faasm/` inter-node result CSVs carry only time today — their
    run harnesses (e.g. `wasmem/run_wordcount.py`, `faasm/…`) need the same `peak_mem_mb`
    capture added and a re-run.
  - Add a `peak_mem_mb` (and external-store) column to **every** `results_*.csv` (all four
    series: Cloudburst, RMMap-RDMA, Faasm, WasMem); build an inter-node `mem_footprint`
    analog so memory bars sit alongside the time bars.
- ~~Regenerate the other datasets~~ ✅ **DONE — all 6 datasets generated** (§3).
- **Port the 5 remaining workloads to Cloudburst + RMMap**, reusing the driver patterns:
  extend the Cloudburst executor (`k8s/cloudburst/`) and, for RMMap, the relevant harness
  with each workload's stage bodies.
- **RMMap variant per workload:** WordCount ✓. TeraSort is also flat-data → the RDMA-read
  path (`rmmap-rdma-wordcount/` generalized) applies cleanly. Matrix/FINRA/ML have richer
  inter-stage objects → RDMA still removes the KVS staging but not the deserialize (no
  pointer remapping without MITOSIS); decide per workload whether to report RDMA, ES, or
  both, and label the gap.
- Keep the figure's RMMap bar = RDMA where available; ES rows remain in the CSVs for reference.
