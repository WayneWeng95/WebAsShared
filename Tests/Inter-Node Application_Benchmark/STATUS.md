# Inter-Node Application Benchmark — Status

WasMem (our auto-placement framework) vs the Faasm baseline, on the same 4-node
cluster, same workloads, measured the same way. Last updated **2026-06-21**.

- `faasm/`   — Faasm baseline drivers (per-node agents + Redis state). `results*.csv`.
- `wasmem/`  — WasMem drivers (coordinator + RDMA state). `results*.csv`, `dags/`.
- `scripts/` — `gen_variants.py` + per-workload DAG generators (balanced policy).

## Measurement methodology (both frameworks)

Goal: **end-to-end job makespan**, excluding the offline scheduler/partitioner.

| | Faasm | WasMem |
|--|--|--|
| Timed window start | `t0` **before** the Redis upload of input | coordinator `job_start` (after offline partition + worker-DAG dispatch) |
| Timed window end | last Faaslet completes (result written) | all workers complete; `save` Output node written |
| Makespan source | wall clock around launch→poll loop | coordinator Job Summary `total wall time` |
| `total_job_ms` | Σ per-**process** Faaslet wall times | Σ per-**node** executor durations |
| Excluded | (no scheduler) | the partitioner (offline), worker-DAG dispatch |
| Per-rep hygiene | `FLUSHDB` before each rep (cold Redis) | fresh submit each rep |

⚠️ **Two asymmetries to keep in mind when comparing:**
1. **Input staging:** Faasm's makespan *includes* the Redis upload; WasMem's *excludes*
   the initial RDMA corpus staging (it happens before `job_start`). WasMem still wins
   at 4 GB despite this.
2. **`total_job_ms` granularity:** WasMem = one executor/node (≈ few × makespan);
   Faasm = one process/worker (scales with worker count). **Compare makespan, not
   total_job_ms.**

Both frameworks run **AOT** (Faasm `wc.cwasm` via `wasmtime compile`; WasMem `--aot`
precompiles `guest.wasm`→`.cwasm`). Tokenizers are **aligned** (see fixes).

## Workload status

| Workload | Faasm | WasMem | Head-to-head |
|----------|-------|--------|--------------|
| **WordCount** | ✅ 4 GB @ 60, 15 reps | ✅ 4 GB @ 60 (overlap fix), 15 reps | ✅ 4 GB: WasMem 13.8±0.1 s **beats** Faasm 15.7±0.1 s (1.14×) |
| **FINRA** | ✅ 5M @ 60 hybrid-shard (58 wkrs), 15 reps | ✅ 5M @ 60 (197 MB, under OOM ceiling), 15 reps | ✅ 5M: WasMem 10.25±0.14 s **beats** Faasm 13.08±0.22 s (1.28×); OOMs at 10M |
| **ML training** | ✅ 600k–6M @ 60 (new sgd baseline) | ✅ 600k @ 60 (1 epoch), 15 reps | ⚖️ 600k: WasMem 0.85 s **wins** 1.41× → **6M: Faasm wins** (crossover) |
| **ML inference** | ✅ 600k–6M @ 60 (new infer baseline) | ✅ 600k @ 60, 15 reps | ⚖️ 600k: WasMem 0.69 s **wins** 1.45× → **6M: Faasm wins** (crossover) |
| **Matrix** | ✅ N4096 @64 (new matblock baseline) | ✅ N4096 @64 (8×8, widened layout), 15 reps | ⚠️ **Faasm 1.19×** (9.67±0.50 vs 8.12±0.22 s) — compute-bound; heap-bound ceiling at N=4096 |
| **TeraSort** | ✅ 1.2 GiB @ 4 (new WC-shaped baseline), 15 reps | ✅ 1.2 GiB @ 4 (input-shard + MR2 2 GiB), 15 reps | ✅ **1.2 GiB: WasMem 11.2±0.6 s beats Faasm 15.5±0.4 s (1.39×)**; cap ~1.2 GiB, fan N==P=4 |

## Results so far

**WordCount — head-to-head, 4 GB @ 60 mappers (15/node), 4 nodes, 15 reps** (`{wasmem,faasm}/results_wordcount.csv`):

| System | Makespan (mean ± std, 15 reps) | Occurrences | ✓ |
|--------|--------------------------------|-------------|---|
| **WasMem** | **13,834 ± 100 ms** | 715,227,097 | ✓ |
| Faasm | 15,724 ± 131 ms | 715,227,097 | ✓ |

**WasMem wins 1.14×** — single bounded config (4 GB @ 60, 15 reps). Occurrence counts match
exactly. The RDMA zero-copy map→reduce gather beats Faasm's serialized-KV shuffle, with the
node-0 overlap fix (deferred RemoteRecv join) keeping the gather off the critical path.
(The earlier reps-3 median read 12.0 s → 1.31×; the 15-rep mean 13.8 s is higher and more
representative, so the honest win is **1.14×**. NB Faasm pays its 4 GB Redis upload in the
timed window, by design — the real KV cost; WasMem excludes RDMA staging. Smaller sizes
dropped per user — only the 4 GB head-to-head is kept.)

**FINRA — head-to-head, 5M trades @ fanout 60 (hybrid shard → 58 workers), 4 nodes, 15 reps** (2026-06-21) (`{wasmem,faasm}/results_finra.csv`):

| System | Makespan (mean ± std, 15 reps) | Violations | ✓ |
|--------|--------------------------------|-----------|---|
| **WasMem** | **10,251 ± 138 ms** | 2,271,415 | ✓ |
| Faasm | 13,079 ± 224 ms | 2,271,415 | ✓ |

**WasMem wins 1.28×**, single bounded config (5M @ 60, 15 reps). Now a FAIR
60-vs-60 comparison: previously WasMem ran 60-way (hybrid shard) but Faasm was only 8-way;
this session the **Faasm side was brought to the same hybrid sharding** — the 5 stateless
rules each split into S=11 disjoint slices (header prepended so `finra.cwasm`'s header-skip
lands right; per-shard counts sum exactly) + 3 stateful rules full-data = 58 workers
(`faasm/run_finra.py --fanout 60`, no module change). Violations match exactly.

**Why fan-out doesn't separate them more:** the 3 **unsharded stateful rules**
(wash/spoofing/concentration) each re-scan the full trade set and cap the makespan on both
sides — the extra 55 stateless Faaslets just add launch + Redis overhead for Faasm. WasMem
hits the same stateful wall but pays it in cheap SHM reads (vs Faasm's full-set Redis reads),
which is the 1.28× edge. (Earlier reps-3 runs read ~1.45×; the 15-rep mean±std is higher and
more representative — WasMem 8.99 s→10.25±0.14 s, Faasm 13.08±0.22 s.)

**10M dropped** (per user): WasMem FINRA OOMs at 10M (the stateful rules' `chain_read_all`
loads the whole 403 MB set into wasm32 linear memory → `rust_oom` at ~384 MB; 5M=197 MB fits),
so there's no WasMem counterpart. The fix would be to stream/shard the stateful rules in the
guest (open item). Faasm alone does 10M @ 60 (streams stdin), but it's not a head-to-head.

**TeraSort — head-to-head, 4-node, 32 MB & 1 GB (new this session, 2026-06-21)** (`{wasmem,faasm}/results_terasort.csv`):

| Size | WasMem makespan mean±std | Faasm makespan mean±std | Winner | gate (records, keysum, sorted) | Reps |
|------|--------------------------|-------------------------|--------|--------------------------------|------|
| **1.2 GiB** (1229 MB) | **11,180 ± 647 ms** | 15,496 ± 352 ms | **WasMem 1.39×** | 12886999, 8312165245, 1 | **15** |

Single bounded config: **1.2 GiB @ fan-out 4 (N==P), 15 reps.** Gates match exactly across
systems and against the WasMem `--nodes 1` ground truth. **WasMem wins 1.39×** — the RDMA
zero-copy gather dominates Faasm's full-dataset Redis serialization even though WasMem's merge
is still a **centralized single-reducer** (node 0 sorts the whole dataset, vs Faasm's
*distributed* 4-way merge). So WasMem leads *despite* the merge disadvantage — conservative
for WasMem; the true distributed all-to-all (§gaps) would widen it further. (At small sizes,
e.g. 32 MB, the two are a tie — the gap opens with data size.)

**Fan-out is capped at N==P=4** (unlike FINRA's 58 / Matrix's 64): the input-shard path that
enables 1 GB+ needs exactly one partition worker per node (`--shard-input --fanout 60` →
15 workers/node all read the same node-slice → 15× record duplication, verified offline), and
the replicate path that supports N>P caps at ~512 MB + risks the N>P deadlock. More fan
wouldn't help anyway — the centralized merge is the bottleneck, not partition parallelism.

**1.2 GiB is the largest size run** (the practical ceiling under the current 1.5 GiB SHM cap).
Pushing to 1.5 GB was scoped and rejected: it needs widening the SHM window
(`CAPACITY_HARD_LIMIT` + the coupled wasm Memory `min` in `worker.rs`), which shrinks the guest
heap node 0's sort needs — too marginal/invasive. (1 GB head-to-head also dropped per user;
its input `records_1024mb.txt` was removed in cleanup.)

**Reaching 1 GB required two framework changes this session (it was capped at ~512 MB):**
the original TeraSort DAG `replicate`d the whole input on every node (1.5 GiB SHM ceiling)
and `gather`ed the full dataset into one node-0 SHM slot. Both walls were lifted:
1. **Per-node input sharding** — new additive guest fn `ts_partition_local`
   (`Executor/guest/.../terasort.rs`) reads the node-local input slice via `for_each_input`
   (the host slices slot 0 across nodes, no `replicate`/`ts_distribute`); new
   `gen_terasort_ap_dag.build_dag(..., shard_input=True)` + `gen_variants --shard-input` +
   `run_terasort.py --shard-input`. Cuts per-node footprint from the whole file to 1/P.
2. **RDMA MR2 512 MiB → 2 GiB** (`Executor/common/src/lib.rs` `RDMA_MR2_INITIAL_SIZE`) —
   node 0's centralized gather reserves all N incoming transfers concurrently in one MR2
   bump region; 512 MiB overflowed at ~1 GB (`mr2_reserve: no room`). 2 GiB covers the
   gather up to the ~1.5 GiB SHM-slot ceiling.

**Ceiling now ~1–1.3 GB** (node 0's post-gather dataset still lands in a single SHM slot
capped at ~1.5 GiB usable). **2 GB confirmed infeasible** on the centralized path — needs
the true distributed merge (each owner gathers only its 1/N range). See [[terasort-shm-ceiling]].

**Baseline note:** the Faasm side is new code this session — `faasm/run_terasort.py` +
`faasm/ts_faaslet.py` reuse the intra-node `ts.cwasm` (partition/merge filter) over Redis
buckets (the serialized all-to-all shuffle), distributed via `deploy.sh distribute`.

**Deploy note:** the input-shard guest + MR2 host changes are built on node 0 and pushed to
all workers via the Faasm `/upload` (`deploy.sh distribute`), NOT committed/rebuilt per node.
For durability, commit the source + `./build.sh` on every node.

**Matrix (SUMMA) — head-to-head, 4-node, N=4096 @ fanout 64 (8×8), 15 reps (2026-06-21)** (`{wasmem,faasm}/results_matrix.csv`):

| N | fanout | WasMem makespan mean±std (GFLOP/s) | Faasm makespan mean±std (GFLOP/s) | Winner | checksum |
|---|--------|-----------------------------------|-----------------------------------|--------|----------|
| **4096** | **64 (8×8)** | **9,674 ± 496 ms (14.2)** | 8,120 ± 217 ms (16.9) | **Faasm 1.19×** | 1391095867672 |

(Makespan = **mean ± sample-std over 15 reps** — switched from median 2026-06-21; all 6
re-run. Means track the earlier medians closely, low std → conclusions unchanged.)

Single bounded config: **N=4096, full-cluster 64-way (8×8 grid, 16 blocks/node), 15 reps.**
Checksums match exactly across systems and against ground truth (gate = f64 Σ of all C
entries, exact integer, order-independent). **Faasm wins 1.19×** — Matrix is the workload that
runs *opposite* to the data-movement ones (WordCount/FINRA/TeraSort), and it's *expected*:
Matrix is **compute-bound** (N³ FLOPs) with an **identical naive-ikj kernel** both sides
(`matrix.rs` mat_block ≡ intra `matblock.rs`, same zero-skip, same integer data), so WasMem's
RDMA zero-copy data path — its whole advantage — is a negligible fraction of the work. WasMem's
per-block compute is somewhat slower; likely (a) the guest's **shared wasm linear memory**
(mandatory for the SHM page-chain) carries a per-load/store codegen tax that only bites in a
tight FLOP loop, and (b) `tile` re-slices A/B panels on **every** node. **But at full 64-core
parallelism the gap is only 1.19×** (it was 1.92× at the under-utilized N=2048/16 earlier) —
the tax is real but WasMem stays competitive once the cluster is saturated. Intra-node WasMem
matrix *wins* (SHM, `results_aot.csv`), so the gap is inter-node-specific. **Fairness caveat:**
"same kernel" holds at source but not realized speed — a genuine WasMem-unfavorable axis.

**Why bounded at N=4096:** larger N OOMs the **guest heap**. Matrix is GUEST-HEAP-bound, not
SHM-bound: each block reads its A+B panels into wasm heap (~75 MB/block at N=6144), and 16
concurrent blocks/node far exceed the **0.5 GiB guest heap** (the half of the wasm32 window
not given to the SHM page-chain). N=6144 @ 64 fails (`Success: false`, workers die); N=4096 @
64 (~34 MB/block × 16 ≈ 0.5 GiB) is right at the edge and the practical ceiling. N=8192 also
needs widening the `n` field (caps at 8191). So 4096 is Matrix's max for full-cluster runs.

**Code (new this session, additive):** packed-arg layout widened r,c 3→4 bits
(`matrix.rs::unpack_rcn` + `gen_matrix_ap_dag.py`) so 8×8 (64 blocks) fits; new N=4096 matrix
(`TestData/matrix/{A,B}_4096.bin`). WasMem driver `wasmem/run_matrix.py`; Faasm baseline
`faasm/run_matrix.py` + `matblock_faaslet.py` reuse the intra `matblock.cwasm` over Redis
panels/blocks. Guest rebuilt + pushed to workers via `deploy.sh distribute` (not committed).

**ML inference & ML training — head-to-head, @ fanout 60, 4-node (new this session, 2026-06-21)** (`{wasmem,faasm}/results_ml_{inference,training}.csv`):

| Workload | Size | WasMem | Faasm | Winner |
|----------|------|--------|-------|--------|
| **ML inference** (MNIST predict fan) | 600k (15 reps) | **692 ± 19 ms** | 1,001 ± 27 ms | **WasMem 1.45×** |
| | 6M (3 reps) | 3,533 ± 96 ms | 3,092 ± 159 ms | Faasm 1.14× |
| **ML training** (1-epoch SGD grad fan) | 600k (15 reps) | **846 ± 130 ms** | 1,189 ± 45 ms | **WasMem 1.41×** |
| | 6M (3 reps) | 5,336 ± 100 ms | 4,748 ± 59 ms | Faasm 1.12× |

Gates match exactly (inference prediction_checksum, training weight_checksum=1232 at any N —
the SGD step is N-normalized), accuracy 74.3% both. **Both ML workloads CROSS OVER with data
size:** WasMem wins at 600k, Faasm wins at ≥3M (3M read Faasm ~2×/1.6× — even wider). Two new
Faasm baselines (`faasm/run_ml_{inference,training}.py` + `{infer,sgd}_faaslet.py` reuse the
intra `infer_predict.cwasm`/`sgd_grad.cwasm` over Redis frames); WasMem drivers
`wasmem/run_ml_{inference,training}.py` (homogeneous fan, scales to 60 like WordCount/FINRA).

**Why the crossover (investigated, key finding):** ML has two costs — (1) data-movement
(broadcast model + gather predictions/gradients), WasMem's SHM/RDMA wins, dominates at small
data → WasMem wins 600k; (2) **per-record compute** (parse each CSV line + integer forward
pass/gradient) over MILLIONS of tiny records, dominates at ≥3M → WasMem loses ~2×/sample
(~28 µs vs Faasm ~13 µs for ~180 int ops). **Two guest parse-optimizations were tried and
neither helped (≤6%):** stack-array (no per-record `Vec`) and a byte-level parser (no
per-record `from_utf8`/`str`). So the cost is NOT alloc/parse — it's STRUCTURAL: `for_each_
stream_record` walks the SHM **page chain per record** (6M Acquire-loads), and the forward
pass runs in **shared wasm memory** (the same shared-mem tax as Matrix). Faasm avoids both
(non-shared wasm, contiguous stdin shard). The byte-parse changes (`{ml_inference,ml_training}.rs
*_parse_into`) are kept as a minor cleanup. **Takeaway:** WasMem's zero-copy SHM win inverts
into a loss for per-record compute over many small records (ML ≥3M, Matrix) — the opposite
regime from its data-movement wins (WordCount 4 GB, TeraSort). See [[ml-crossover-compute-bound]].
(NB: at 6M with both parsing in-wasm — a fair test before reverting Faasm to numpy — Faasm was
~2.6× faster, since the numpy `make_frame` is a node-0 timed-window bottleneck for Faasm at 6M.)

## Fixes made this session (with locations)

1. **Faasm measurement boundary** — `faasm/run_wordcount.py`, `run_finra.py`,
   `faasm_lib.py`, `README.md`: `t0` moved before the Redis upload (end-to-end),
   `FLUSHDB` per rep, result saved to `TestOutput/`, added `total_job_ms` column.
   Fixed missing `import time` in `run_finra.py`.
2. **Tokenizer alignment** — `Tests/Intra-Node Application_Benchmark/WordCount/baseline/faasm/demo/wc.rs`:
   Faasm now tokenizes like the WasMem guest (whitespace-split, strip non-alpha within
   token: `don't`→`dont`). Rebuilt `wc.cwasm`, staged to workers via agent `/stage`.
3. **WasMem >2 GB RDMA staging bug** — `NodeAgent/agent/src/coordinator.rs`
   (`stage_shared_inputs_rdma`): a single RDMA WRITE > 2 GiB (IB max-message limit;
   `ibv_sge.length` is u32) was silently dropped (SUCCESS completion, no fallback), so
   ≥3 GB corpora produced only node 0's slice. Fix: **chunk the WRITE into ≤1 GiB
   segments**. Rebuilt `node-agent` (node 0 only).
4. **WasMem node-0 convergence bottleneck** — `Executor/host/src/runtime/dag_runner/mod.rs`:
   RemoteRecv threads were joined at the end of their spawn wave (wave 0), making the
   gather a **barrier in front of node 0's compute**. Fix: **defer the RemoteRecv join
   to the consuming wave** (`pending_recv`), so node 0 computes while peers' partials
   stream in. 4 GB @ 60: 20.7 s → **12.0 s** (1.72×), per-node durations now balanced.
   Rebuilt `Executor/.../host` (node 0 only).
5. **WasMem WordCount driver** — `wasmem/run_wordcount.py` + `run.sh` + `README.md`.
   Note: `word_count` is a pinned **control** in `gen_variants.py` (`--fanout` inert);
   set the DAG node `map_n.fanout` directly to widen. `--weights` enables the
   `weighted` policy (e.g. `6,11,11,12` to offload node 0).
6. **WasMem FINRA driver** — `wasmem/run_finra.py` (uses `gen_variants finra` hybrid
   sharding; fanout 60 → 60 `finra_audit_rule` workers, 15/node).

## Known issues / TODO

- [x] **WasMem FINRA runs up to 5M (197 MB).** ✅ 5M validated 2026-06-21 (see results).
  The OOM ceiling is still real **at 10M / 403 MB**: `finra_fetch_private`/`fetch_public`
  (the 3 **stateful** rules, NOT sharded by the hybrid) call `api::stream_area::chain_read_all`
  → load the *entire* trades set into wasm32 linear memory → `rust_oom` (SHM caps at 512 MiB).
  Faasm survives 10M because `finra.cwasm` **streams** stdin. To match Faasm's 10M reach (and
  to get inter-node *scaling* — see the 5M scaling caveat above), still need to make the
  stateful rules stream/shard the trades in `Executor/guest`. Until then, 5M is the largest
  valid WasMem FINRA point; 10M remains Faasm-only.
- [x] **WordCount bounded at 4 GB @ 60, 15 reps, mean±std** (smaller-size curve dropped per
  user). WasMem 13.8±0.1 s vs Faasm 15.7±0.1 s (1.14×). A size-sweep curve could be re-added
  later if wanted, but the headline is the single 4 GB point.
- [~] **Faasm FINRA** is no longer capped at 8-way — now hybrid-sharded to 58 workers
  (`run_finra.py --fanout 60`); the original 8-way note below is historical. To
  scale parallelism past 8 it would need to shard trades within each rule.
- [ ] **Remaining workloads** (ml_training, ml_inference, matrix): run the
  Faasm-vs-WasMem head-to-head. Gen scripts exist in `scripts/`. (TeraSort ✅ done 32 MB.)
- [ ] **TeraSort: larger sizes + the real shuffle.** Only 32 MB staged (a tie). Generate
  bigger records files to find the crossover — but expect WasMem to *lose* with size until
  the **true all-to-all transpose** lands (non-blocking/phased RDMA, §gaps); the current
  centralized gather makes node 0's single-reducer merge the bottleneck. Drivers:
  `{wasmem,faasm}/run_terasort.py`; Faasm Faaslet wrapper `faasm/ts_faaslet.py` (reuses
  intra `ts.cwasm`); both N==P (fanout=nodes). N>P needs a `ts_demux` guest (§gaps).
- [ ] **Durable deploy.** The coordinator (RDMA-staging) and executor (overlap) fixes
  are built **only on node 0** (correct, since the changed code paths only execute on
  the coordinator/gather node). Workers run older binaries. For durability: commit the
  source changes and rebuild/redeploy all nodes via `build.sh` + sync.
- [ ] Consider a `--expect` (true-count) gate on every WasMem run — rep-consistency
  alone masked both the >2 GB drop and the FINRA stale-result-file.
- [ ] **2 MB (huge) pages instead of 4 KB — already MEASURED (harness-only) in
  `Tests/PageSize/`; not yet a real engine option (clarified 2026-06-21).**
  The 2 MiB page was tested in the **intra-node StateSync read-write** experiment:
  `Tests/PageSize/run.sh` sweeps page size {4 KiB, 64 KiB, **2 MiB**} × transfer
  {16 KiB…128 MiB} through the `shm_statesync` harness's `--page-bytes N` flag
  (`Executor/connect/examples/shm_statesync.rs`). **Findings** (`Tests/PageSize/results.csv`):
  - **Page size is a PUT lever.** At 128 MiB, copy-PUT climbs **4.45 → 5.58 → 7.79 GiB/s**
    for 4 KiB → 64 KiB → 2 MiB (toward the ~8.9 GiB/s flat-memcpy ceiling) — bigger pages =
    fewer, larger `memcpy` spans hitting prefetch / non-temporal-store sweet spots.
  - **GET is page-insensitive** in this probe: zero-copy splice GET is flat ~0.2 µs (pointer
    move); copy GET is bound by the materialize bandwidth regardless of page size. NB this
    probe is a *single bulk* GET, **not** the per-record `for_each_stream_record` walk — so
    it does **not** by itself settle whether bigger pages help the ML-≥3M / Matrix per-record
    page-chain-walk cost ([[ml-crossover-compute-bound]]); that walk's per-page Acquire-load
    count would still drop with fewer/larger pages, but it's unmeasured here.

  **But this is harness-only — the real engine is NOT modified.** `common::PAGE_SIZE` is
  still a hard-coded compile-time `const = 4 * KIB` (`Executor/common/src/lib.rs:82`); no
  runtime flag, no `agent.toml` setting, no feature gate; the SHM file (`Executor/host/src/shm.rs`)
  is mmap'd plain `MAP_SHARED` (no `MAP_HUGETLB`/THP). The README explicitly keeps 4 KiB as
  the engine default because enlarging it coarsens allocation/splice granularity and wastes
  memory on small records (a "jumbo span" fast-path for large single records is the noted
  future direction). **To make 2 MiB real in the engine, two routes:**
  1. **THP-back the SHM (4 KB logical pages unchanged) — cheap, additive, separate from the
     PageSize lever.** `madvise(addr, size, MADV_HUGEPAGE)` after each `map_into_memory`/
     `expand_mapping` in `shm.rs` (or `MAP_HUGETLB | MAP_HUGE_2MB` via a `hugetlbfs` mount).
     Page-chain ABI + guest code untouched; only the OS backing uses 2 MiB frames, cutting
     TLB pressure on the hot walk. The PageSize PUT result is about *logical* chunk size, so
     this is a complementary (TLB) lever, not the same one.
  2. **Resize the logical page to 2 MiB — invasive, matches the PageSize-measured PUT win but
     hits its small-record memory cost.** `Page` is `#[repr(C, align(4096))]` + a
     `size_of::<Page>() == PAGE_SIZE` static assert (`lib.rs:324,339`); literal `4096`s in
     `Executor/guest/src/api/shared_area.rs` (150/169/230/283) bypass `PAGE_SIZE`;
     `BUCKET_COUNT = PAGE_SIZE/PAGE_ID_SIZE` balloons; every slot/chain-node consumes ≥2 MiB
     even for tiny payloads (2048 stream slots × 2 MiB; wasm32 1.5 GiB window → ~768 pages).
     **Not** a drop-in toggle. The PageSize numbers quantify its upside; the README's
     small-record waste is its downside.

## Operational notes (cluster)

- 4 nodes (`10.10.1.2`=coord/node0, `.1`/`.3`/`.4`=workers), 16 cores each, RoCE RDMA.
- **SSH to workers is blocked** — distribute files to workers via the Faasm agent
  `/stage` endpoint (base64); the raw `/upload` needs the newer agent (workers run a
  stale one). Coordinator restarts are the user's job (workers exit on coordinator
  disconnect and can't be SSH-restarted).
- **Which fixes need which redeploy:**
  - coordinator.rs (RDMA staging) → rebuild `node-agent`, restart node-0 coordinator.
  - dag_runner (overlap) → rebuild `Executor/.../host` only; agent spawns it fresh per
    job, **no coordinator restart needed**.
  - `wc.cwasm` (tokenizer) → rebuild on node 0, `/stage` to workers; no restart (each
    Faaslet launch reads the file fresh).
- Build with the repo toolchain: `RUSTUP_HOME=/opt/myapp/.rustup
  CARGO_HOME=/opt/myapp/.cargo PATH=/opt/myapp/.cargo/bin:$PATH` (system `/usr/bin/rustc`
  lacks the wasm target).
- TestData on `/` has limited headroom (~6 GB free at 4 GB corpus). Redis persistence
  is off (no disk spill).
