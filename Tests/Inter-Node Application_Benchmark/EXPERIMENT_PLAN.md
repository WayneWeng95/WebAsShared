# Inter-Node Application Benchmark — design & progress tracker

**Status: DESIGN / SCAFFOLDING (started 2026-06-18).** Nothing run yet. This file
is the single source of truth for *what we are building, why, and how far along we
are*. Update the checkboxes in [§9](#9-progress-tracker) as work lands.

---

## 1. What this experiment is (and how it differs from the sibling)

We already have an **Intra-Node** application benchmark
(`../Intra-Node Application_Benchmark/`, formerly mis-named "Inter-Node"): the
same five workloads run on a **single box**, WasMem driven by `node-agent run`,
state moving through the **shared-memory page-chain** (`topo=intra-shm`), compared
against single-host ports of RMMap / Faasm / Cloudburst.

**This** experiment is the genuine **Inter-Node** version: the *same five
workloads and the same three baselines*, but the work is **distributed across the
4-node cluster**. For WasMem, inter-stage state crosses the network through the
**zero-copy RDMA page-chain** instead of SHM. For the baselines, state crosses the
network through a **serialized KV store** (shared Redis). The scientific question
is identical to the intra-node one, lifted one level up the memory hierarchy:

> When a dataflow stage's output must reach the next stage **on another machine**,
> WasMem moves it RDMA zero-copy (serialization-free); the baselines serialize it
> into a network KV and the consumer deserializes it back. The gap should **widen
> with data size** and with **cross-node edge count**, exactly as it does intra-node
> across the SHM-vs-local-KV boundary — but now the contrast is RDMA-page-chain vs
> network-KV, which is the headline of the system.

**Same workloads as the intra-node suite** (per
`../Intra-Node Application_Benchmark/README.md`):

| # | Workload | Family | Primary baseline | Secondary |
|---|----------|--------|------------------|-----------|
| 2 | WordCount    | data-parallel text       | RMMap (native twin) | Faasm, Cloudburst |
| 3 | FINRA        | multi-stage stateful DAG | RMMap (native)      | Faasm, Cloudburst |
| 4 | Matrix       | distributed compute      | Faasm (D&C)         | RMMap, Cloudburst (SUMMA) |
| 5 | ML training  | distributed SGD          | RMMap `ml-pipeline` | Faasm (HOGWILD!), Cloudburst |
| 6 | ML inference | MNIST inference          | RMMap (native MNIST)| Cloudburst, Faasm |
| 7 | **TeraSort** *(new)* | data-parallel **all-to-all shuffle** | — (port all 3) | RMMap-ES, Faasm-like, Cloudburst | distributed sort; the one workload with a real shuffle. **PageRank is the backup** (graph/iterative). |

---

## 2. WasMem execution = the Scheduling-Policy path (not `node-agent run`)

Per the directive: **our platform's execution code mirrors the Scheduling-Policy
experiment** (`../Scheduling_Policy/`), *not* the intra-node `run.sh` (which uses
the single-box `node-agent run` local executor). That is exactly the right driver
for inter-node because it is the **cluster control plane**: pre-partition offline,
then submit a finished ClusterDag to the coordinator.

**Scale plan: 4 nodes now (small-scale bring-up) → 9 nodes for the full run.** The
target cluster for the headline numbers is **9 compute nodes** (+ the dedicated
Redis box, §3). We do **not** have that yet — so the first pass is a **small-scale
shakeout on the existing 4-node cluster**: validate the full pipeline end-to-end
(WasMem cluster submit, all three baseline frameworks, correctness gates, figures)
at N=4, then scale the same harness to 9 nodes by editing `agent.toml`'s `[cluster]
ips` and the `--nodes`/fanout knobs — **no code change**, the partitioner and the
sweeps are already node-count-parametric. Numbers below describe the **current
4-node** config; treat them as provisional until the 9-node run.

**The current 4-node cluster** (from `NodeAgent/agent.toml`):

```
node-0 = 10.10.1.2 (coordinator)   node-2 = 10.10.1.3
node-1 = 10.10.1.1                  node-3 = 10.10.1.4
```

Each node runs `node-agent start --config NodeAgent/agent.toml`; the coordinator
RDMA-stages shared inputs to the workers. The 9-node config is the same file with
9 IPs and `total_nodes`/`--nodes 9`.

**The per-cell driver loop** (reused verbatim from `wc_size_sweep.sh` /
`finra_sweep.sh` / `ml_training_sweep.sh`):

```
gen_variants.py <wl> <policy> --nodes 4 --input <data> --fanout <F> --pack-cap 16 --out <sym>
partition <sym> --nodes 4 > <cdag>        # OFFLINE → embedded placement authoritative
node-agent submit --config agent.toml --dag <cdag>   # coordinator runs it across nodes
# parse the run log: "total wall time: Xms" (makespan) + per-node "node N (...): Yms"
```

- **Pre-partition offline** so the embedded `placement_policy` is authoritative —
  on a live cluster the coordinator's SCX hints would otherwise override it
  (`splitter.rs`, `hints.or(policy_hints)`).
- We are **not** sweeping placement policy here (that is the Scheduling-Policy
  experiment's job). For the cross-system comparison we fix a **single sensible
  placement** per workload (the one that runs deadlock-free and distributes the
  fan): `balanced` for the bandwidth-bound fan-out workloads, `pack` where a
  distributing policy still deadlocks (see [§8](#8-known-gaps--risks)). The policy
  choice is a documented knob, not an axis.
- **Metrics** come from the coordinator's run log + `NodeAgent/agent/src/metrics.rs`
  (`/tmp/node_agent_metrics.jsonl`) + the `[DAG][timing]` lines. Headline numbers:
  near-zero serialization cost, RDMA bytes ≈ 1× the cross-node working set.

### What's reusable vs what's new (WasMem side)

| Workload | Cross-node auto-placement DAG | gen_variants support | Status |
|----------|-------------------------------|----------------------|--------|
| WordCount    | `DAGs/symbolic_dag/word_count_auto_placement.json` ✓ | yes (control) | **reuse Scheduling-Policy harness directly** |
| FINRA        | `DAGs/symbolic_dag/finra_auto_placement.json` ✓ | yes (hybrid shard) | **reuse** (all 4 policies validated cross-node) |
| ML training  | `DAGs/symbolic_dag/ml_training_auto_placement.json` ✓ + `gen_sgd_ap_dag.py` | yes | **reuse** (local-combine gather) |
| Matrix       | **none** — must author `matrix_auto_placement.json` | **must add** | **NEW: cross-node SUMMA DAG** |
| ML inference | **none** — must author `ml_inference_auto_placement.json` | **must add** | **NEW: cross-node predict fan** |

> Matrix & ML-inference were also out of scope for the Scheduling-Policy
> experiment. Their cross-node DAGs are the **real new framework-side work** here —
> author them *additively* (no edits to existing guest stages, per the
> backward-compat rule), mirroring how `finra`/`ml_training` were lifted to
> auto-placement.

---

## 3. How the three baselines run inter-node

Same systems as intra-node, but **distributed across the same 4 compute nodes** so
the comparison is on identical hardware/data. **Deployment strategy (revised
2026-06-18): run each baseline on its real distribution mechanism**, not an
abstracted local stand-in:

- **The two framework-based systems run on their framework.** RMMap → **Knative**;
  Cloudburst → **Kubernetes**. These are the orchestration layers their papers ship
  on, so we stand them up properly across the cluster rather than emulate them.
- **Faasm is clean-slate → we supply a per-node local agent.** Faasm has no
  orchestration framework of its own (Faaslets + Faabric), so we run a small
  **local agent on each of the 4 nodes** that launches/places Faaslets there (the
  per-node analogue of what Knative/k8s do for the other two). This is the main
  new baseline-side engineering for inter-node.
- **All baselines use Redis for inter-node state communication → no extra transport
  work.** Each system already moves cross-stage state through a KV; pointing them at
  one shared Redis gives us the distributed data path for free. The serialized blob
  crossing the network is exactly the cost WasMem's RDMA page-chain moves zero-copy.

> **Why these are "their framework" (per the papers).**
> - **RMMap / dmerge** (EuroSys '24, SJTU IPADS) — RDMA remote-memory-map primitive
>   **integrated into Knative**; its headline is speeding up *Knative* workflows.
> - **Cloudburst** (VLDB '20, Berkeley/Hydro) — custom scheduler + Anna KVS, deployed
>   as Docker containers managed by **Kubernetes** (+ EC2 autoscaling).
> - **Faasm** (ATC '20, Imperial) — **clean-slate**: Faaslets (WASM/SFI) + Faabric;
>   Knative is Faasm's *baseline competitor*, not its substrate → hence the per-node
>   agent we add.

### Dedicated Redis machine (5th box)

Redis runs on a **separate, dedicated machine**, *not* on any of the 4 compute
nodes. This (a) keeps the KV off the compute path so a baseline's store traffic
doesn't steal CPU/RAM from its own workers (and doesn't pollute the per-node memory
sampler), and (b) makes the cross-node hop **symmetric and neutral** — every
baseline worker, on any node, pays the same network round-trip to the same store.
The contrast stays clean: *serialized network KV (to the Redis box) vs WasMem's
RDMA page-chain (node-to-node)*.

**Cluster for this experiment:** *currently* 4 compute nodes (`10.10.1.{2,1,3,4}`,
scaling to **9** for the full run — see the Scale plan in §2) (WasMem
coordinator + workers / Knative + k8s + Faasm-agent for baselines) **+ 1 dedicated
Redis machine**. Record its IP and NIC once; every baseline points `REDIS_HOST` at it.

| System | Native framework (used here) | Inter-node deployment | State path |
|--------|------------------------------|------------------------|------------|
| **RMMap** | **Knative** | Knative `Service`+`Sequence`/`Parallel` flows across the 4 nodes | Redis (ES protocol) — **no MITOSIS kernel module** without explicit authorization |
| **Cloudburst** | **Kubernetes** | Cloudburst executors/schedulers as k8s-managed containers across the 4 nodes | Redis (in place of Anna) |
| **Faasm** | none (clean-slate) | **per-node local agent** launches Faaslets on each node | Redis (Faasm KV backed by Redis) |

Reuse the **existing baseline trees** under
`../Intra-Node Application_Benchmark/<Workload>/baseline/` for the per-stage
function code (mapper/reducer/predict logic is unchanged); the inter-node work is
the *deployment* layer — Knative/k8s manifests for two systems, the per-node agent
for Faasm — plus repointing every `REDIS_HOST` at the dedicated Redis box.

> **Feasibility risk (track in §8).** Standing up real Knative-RMMap and
> Kubernetes-Cloudburst is non-trivial: the intra-node suite abstracted them
> precisely because the pinned RMMap/Knative images and the Cloudburst/Anna stack
> were hard to bring up on a single host (see `../Intra-Node Application_Benchmark/
> EXPERIMENT_RUNBOOK.md` §4). On the cluster we attempt the real frameworks; if one
> cannot be stood up in time, fall back to the intra-node abstracted runner for that
> system **with the gap documented**, not silently.

---

## 4. Metrics & CSV shape

Carry over the intra-node shared columns so figures overlay, and **add the
cross-node metrics** the Scheduling-Policy harness already emits:

- **makespan_ms** — end-to-end latency = coordinator "total wall time" (median of
  ≥5 reps). The headline latency.
- **total_exec_ms** — Σ per-node busy time over **working** nodes (idle nodes
  excluded), the resource-cost axis.
- **throughput** — workload unit / (makespan/1000): MB/s (WordCount/ML-train),
  trades/s (FINRA), GFLOP/s (Matrix), samples/s (ML-inference).
- **rdma_bytes** — cross-node state moved (WasMem; from metrics.rs). The baselines'
  analogue is **KV bytes put/get over the network** (their serialization cost).
- **peak_mem_mb** — private-RSS-+-shared-once, the same metric both sides use
  (see the intra-node `analysis/mem_footprint.md`; the measurement-bug fixes from
  2026-06-17 in `problems.md` apply equally here).
- **nodes_used**, **policy**, **fanout**, **reps**, and the per-workload
  **correctness gate** (occurrences / violations=1952 / checksum / accuracy).

Each workload writes a `results.csv` in this shape; baselines normalize to the same
columns. CSV header convention follows `../Scheduling_Policy/analysis/results_*.csv`.

---

## 5. Fairness rules (carried over + inter-node specifics)

From the intra-node runbook §5 (still binding) plus cross-node additions:

1. **Same cluster, same input data, same N grid** for ours and every baseline.
2. **Correctness first** — counts/violations/checksums must match the single-node
   ground truth at every config before any timing is trusted. Each sweep runs a
   `--nodes 1` ground-truth cell first (as `finra_sweep.sh` does).
3. **AOT for ours** when comparing to AOT baselines (no per-worker JIT penalty).
4. **Run experiments ONE AT A TIME — never overlap systems** (CPU contention +
   the out-of-band memory sampler sums all matching `host` RSS). This matters *more*
   inter-node: a backgrounded sweep on the cluster poisons every node's timing.
5. **Same compute kernel** across systems for kernel-bound workloads (Matrix:
   naive `np.einsum(...optimize=False)` for the Python baselines, never BLAS).
6. **Staging excluded** — RDMA/Redis input staging timed separately, out of compute.
7. **Don't load the MITOSIS kernel module** (RMMap DMERGE) without authorization.
8. **Inter-node fairness:** the baselines' shared Redis lives on a **dedicated 5th
   machine** (off the 4 compute nodes), so every baseline worker pays the same
   neutral network round-trip to the store and the KV traffic never steals compute
   CPU/RAM (nor pollutes the per-node memory sampler). The contrast is purely
   *serialized-network-KV (to the Redis box) vs RDMA-zero-copy (node-to-node)*, not
   topology asymmetry. Record the Redis box + RDMA fabric (`mlx4_0` RoCE) specs once.
9. **Measurement boundary = data-path only (DECIDED 2026-06-18).** Even though
   RMMap/Cloudburst run on their real frameworks (Knative/k8s), the measured
   `makespan` covers **function compute + state transfer only** — the framework's
   control-plane (scheduler, HTTP/CloudEvent envelope, pod/cold-start) is
   **excluded**, the *same boundary the intra-node suite uses*. This keeps WasMem's
   lead attributable to the **substrate** (RDMA page-chain vs serialized network-KV),
   not to Knative/k8s overhead, and means the **intra-node RMMap/Cloudburst CSVs do
   NOT need to be re-run** (both experiments measure the same window; they differ
   only in topology). *Deferred upside:* a "full deployed-system" variant that
   **includes** framework overhead would likely widen our lead further — parked for
   now (results are already strong); if ever added, report it as a **separate
   column**, never folded into `makespan`, and only then would intra-node need a
   matching framework-on-single-node run.

---

## 6. Directory layout (mirrors the intra-node sibling)

```
Tests/Inter-Node Application_Benchmark/
├── EXPERIMENT_PLAN.md            # this file (design + progress)
├── README.md                     # short pointer + per-workload status table  (TODO)
├── <Workload>/                   # one per workload, same 5 as intra-node
│   ├── README.md                 # workload plan + per-baseline inter-node recipe
│   ├── gen_dag.py / variant      # ours: cluster symbolic DAG (reuse gen_variants where possible)
│   ├── run.sh                    # ours: gen_variants → partition → submit → results.csv
│   ├── results.csv               # ours (cluster)
│   ├── plot.py                   # ours vs baselines (StateSync palette, §6 of intra runbook)
│   ├── baseline/                 # reuse intra trees; add NODES=/REDIS_HOST= remote mode
│   └── figs/
└── analysis/                     # cross-workload grids (mirror analysis/plot_bars_grid.py)
```

We **reuse**, not fork: the WasMem driver is the Scheduling-Policy sweep scripts
parameterized for the cross-system comparison; the baselines are the intra-node
baseline trees extended with a remote-dispatch flag. Keep the StateSync plotting
palette and the bars-figure conventions from the intra runbook §6 so the paper is
coherent.

---

## 7. Phase plan

> **Phases 0–4 run first on the existing 4-node cluster** (small-scale shakeout —
> prove correctness + the full cross-system pipeline cheaply), then **Phase 5 scales
> the validated harness to 9 nodes** for the headline numbers. Everything in 0–4 is
> node-count-parametric, so the scale-out is config, not code.

**Phase 0 — infra (once).** Cluster up (coordinator + 3 workers), RDMA fabric
verified, `./build.sh` deployed to every node (rebuilt guest). Stand up the
**dedicated Redis machine** (5th box) and point every baseline at it. Bring up the
two baseline frameworks across the 4 nodes — **Knative** (for RMMap) and
**Kubernetes** (for Cloudburst) — and the **per-node Faasm agent**. Corpora/datasets
staged. Record specs (compute nodes, Redis box, RDMA fabric).

**Phase 1 — WordCount (worked example).** The control + simplest fan. Reuse
`wc_size_sweep.sh` machinery; fix a single placement; sweep size × fanout. Bring up
all three baselines distributed: RMMap on Knative, Cloudburst on k8s, Faasm via the
per-node agent — all pointed at the dedicated Redis box. Validate occurrence gate,
ship 3 figures. This proves the end-to-end cross-system inter-node pipeline (incl.
the framework standup, the riskiest infra).

**Phase 2 — FINRA + ML-training.** Both already lift to auto-placement and gather
deadlock-free (Scheduling-Policy `NEXT_STEPS.md`). Deploy their RMMap/Cloudburst/Faasm
functions on the now-standing frameworks; validate gates (violations=1952; SGD
checksum invariant).

**Phase 3 — Matrix + ML-inference (new DAGs).** Author the cross-node
`matrix_auto_placement.json` (SUMMA block grid across nodes) and
`ml_inference_auto_placement.json` (predict fan across nodes), additively. Bring up
their distributed baselines. Highest-risk phase.

**Phase 4 — cross-workload analysis (at N=4).** The `analysis/` grids + the headline
inter-node table (WasMem RDMA vs network-KV, gap-vs-size). Memory footprint with
the unified private-RSS-+-shared-once metric. These 4-node numbers are the
shakeout/sanity set, not the paper numbers.

**Phase 5 — scale to 9 nodes (the full run).** Extend `agent.toml` to 9 IPs, bring
up Knative/k8s/Faasm-agent on the 5 new nodes (same Redis box), re-run all five
workloads' sweeps with `--nodes 9` + widened fanouts. No harness code change
expected — if any surfaces (e.g. gather width at 9), it's a real finding to fix.
Re-generate every figure + the headline table from the 9-node CSVs.

---

## 8. Known gaps & risks

- **Baseline framework standup (Knative for RMMap, Kubernetes for Cloudburst)** is
  non-trivial — the intra-node suite abstracted them because the pinned RMMap/Knative
  images and the Cloudburst/Anna stack were hard to bring up. Budget time for it;
  fall back to the intra-node abstracted runner *with the gap documented* if a
  framework can't be stood up.
- **Per-node Faasm agent is new code** — a small launcher/daemon on each node that
  places Faaslets locally (no framework does it for Faasm). Keep it thin: launch
  `wasmtime` Faaslets, read/write state via the shared Redis; the state path is
  already Redis so there's no cross-node transport to write.
- **Matrix & ML-inference have no cross-node auto-placement DAG** — the real new
  framework-side work (ours). Author additively; expect the same many-to-many gather
  pitfalls finra/ml_training already hit.
- **TeraSort (#7) is the all-to-all shuffle** — its inter-node variant hammers the
  cross-node **many-to-many gather** path that historically deadlocked under
  distributing policies (Scheduling-Policy `NEXT_STEPS`). Intra-node (SHM) is safe;
  budget gather-stability work for the cluster variant. Folder/plan stubbed at
  `../Intra-Node Application_Benchmark/TeraSort/`.
- **PageRank is the parked backup** for a graph/iterative workload — *not* being
  built now. Higher effort (sparse-graph kernel, convergence, irregular/skewed
  comms), overlaps ML-training's iterative dimension, and not native in any baseline.
  Revisit only if the graph + placement-sensitivity story is wanted; the
  iteration-unroll pattern (ml_training) + `Shuffle`/`dispatch` primitives would be
  the starting point.
- **Distributing-policy deadlock (finra/ml_training):** historically the executor's
  cross-node many-to-many RDMA gather deadlocked under *distributing* policies;
  `gen_variants.py`'s per-machine **local-combine** gather (one transfer/peer) is
  the fix word_count proved. Validate each workload reproduces the single-node
  ground truth before trusting timings. Where a distributing policy still hangs,
  fall back to `pack` and document it.
- **Coordinator robustness:** a hung job currently crashes the coordinator rather
  than timing out cleanly (Scheduling-Policy `NEXT_STEPS.md`). Harden or babysit
  before long unattended cluster sweeps.
- **Peer-failure / RemoteRecv segfault** if a node lacks its local input
  (`problems.md` FUTURE item) — ensure every node has staged inputs first.
- **Memory double-counting** was a measurement bug intra-node (fixed 2026-06-17);
  reuse the corrected samplers (subtract `RssShmem`, read SHM bump offset), don't
  reintroduce `stat -c%s` SHM adds.

---

## 9. Progress tracker

Tick as work lands; keep one line per item.

### Infra
- [x] Confirm inter/intra split; rename old folder → `Intra-Node Application_Benchmark`; fix cross-refs.
- [x] Create this folder + design/plan doc.
- [ ] Cluster stood up, RDMA verified, build deployed to all 4 nodes.
- [ ] Dedicated Redis machine (5th box) up; every baseline `REDIS_HOST` points at it.
- [ ] Baseline frameworks up: Knative (RMMap) + Kubernetes (Cloudburst) across 4 nodes.
- [ ] Per-node Faasm local agent built + deployed on all 4 nodes.
- [ ] Datasets staged; specs recorded (compute nodes, Redis box, RDMA fabric).
- [ ] `README.md` (short pointer + status table) written.

### WasMem driver (Scheduling-Policy-style)
- [ ] Generalize the sweep script (gen_variants → partition → submit → results.csv) into this folder's `run.sh` shape (cross-system columns, not policy-axis).
- [ ] Fix the per-workload placement choice; document it.

### Per-workload (ours + 3 baselines + gate + 3 figs)
- [ ] **WordCount** — ours cluster sweep; RMMap-on-Knative / Cloudburst-on-k8s / Faasm-per-node-agent; occurrence gate; figs.
- [ ] **FINRA** — ours; baselines on frameworks (Redis state); violations=1952 gate; figs.
- [ ] **ML training** — ours; baselines on frameworks; SGD checksum gate; figs.
- [ ] **Matrix** — author `matrix_auto_placement.json`; ours; baselines; checksum gate; figs.
- [ ] **ML inference** — author `ml_inference_auto_placement.json`; ours; baselines; accuracy/checksum gate; figs.
- [ ] **TeraSort (#7, new)** — write `terasort.rs` + `terasort_auto_placement.json`; WordCount-shaped baseline ports; sorted+multiset checksum gate; figs. *(PageRank = parked backup.)*

### Analysis
- [ ] Cross-workload bars grid + headline inter-node table (RDMA vs network-KV, gap-vs-size).
- [ ] Memory footprint (unified private-RSS-+-shared-once).

### Scale-out
- [ ] **4-node shakeout complete** (all 5 workloads + 3 baselines validated at N=4).
- [ ] Cluster + frameworks + Faasm-agent extended to **9 nodes**.
- [ ] Full sweeps re-run at `--nodes 9`; figures + headline table regenerated.

---

## 10. References

- Sibling suite: `../Intra-Node Application_Benchmark/{README.md,EXPERIMENT_RUNBOOK.md}`
  (workloads, baselines, fairness §5, plotting §6, reference numbers).
- WasMem cluster driver to mirror: `../Scheduling_Policy/{README.md,NEXT_STEPS.md,
  gen_variants.py,wc_size_sweep.sh,finra_sweep.sh,ml_training_sweep.sh}`.
- Multi-node Faasm baseline recipe: `../Scheduling_Policy/README.md` §"Baseline".
- Cluster topology: `NodeAgent/agent.toml`. Metrics: `NodeAgent/agent/src/metrics.rs`.
- Open framework items / measurement-bug history: `../../problems.md`.

---

## 11. Code-prep status & per-workload change plan

Snapshot (2026-06-18) of what already exists vs what must change to run each
workload inter-node. **Key finding: the guest WASM functions need NO changes** —
`matrix.rs` already packs `(i,j,r,c,N)` into its block arg and `ml_inference.rs`
packs `(data_slot,out_slot)`, so cross-node placement is expressible purely in the
DAG JSON + `gen_variants.py`. All changes are *additive* (new DAG files, new
`gen_variants` entries, new run/deploy scripts); no edits to existing guest stages
or framework code.

> **UPDATE 2026-06-20 — all three missing workloads CLUSTER-VERIFIED.** ML
> inference, Matrix, TeraSort now run multi-node and pass on the 4-node cluster:
> ground-truth (`--nodes 1`) == distributed (`--nodes 4`) on the fan-out-invariant
> gate — ml_inference `prediction_checksum=155371`, matrix `checksum=2722562338`,
> terasort `records=335544 keysum=216366291 sorted=1` (3/3 stable reps). Verified via
> `scripts/verify.sh`; inputs RDMA-staged to workers automatically. **TeraSort note:**
> the true all-to-all transpose deadlocks the executor's blocking RDMA transport, so
> its multi-node path uses a CENTRALIZED gather (workers route+combine → one transfer
> each → node 0 sorts) — distributed partition, single-reducer merge. True all-to-all
> needs executor-side non-blocking/phased transfers (§8). Two additive guest fns
> (`ts_range_summary`, `ts_finalize`) were added; `ts_merge` + the intra path are
> untouched. Original adoption (offline) details below:
>
> The three workloads now have cross-node auto-placement DAGs + `gen_variants.py`
> support, additively:
> - `DAGs/symbolic_dag/{ml_inference,matrix,terasort}_auto_placement.json` (snapshots)
>   and parametric generators `Tests/Scheduling_Policy/gen_{inference,matrix,terasort}_ap_dag.py`.
> - `gen_variants.py`: `FAN_PREFIX`/`AGGREGATOR` entries for `ml_inference` (`predict_`)
>   and `matrix` (`block_`) reuse the generic fan transform UNCHANGED; `terasort` gets
>   a dedicated builder (the all-to-all shuffle is an N×N transpose, not a fan→1).
>   New flags: `--matrix-n` (matrix dimension), `--fanout` regenerates each fan.
> - **Verified offline** via `gen_variants → partition` for pack/balanced/spread at
>   `--nodes 1/4` and fanouts 4–32 (12 configs, all clean; no regressions in
>   word_count/finra/ml_training). ml_inference & matrix reproduce ml_training's exact
>   deadlock-safe pattern (per-machine local-combine, ONE RemoteSend/peer, reduce/
>   assemble arg auto-injected). TeraSort at **N == nodes** is one partition + one
>   owner per node — each directed node-pair carries exactly ONE stream (the
>   partitioner adds deadlock-prevention ordering for the bidirectional transpose).
> - **Cluster-validation caveat (TeraSort):** N == P is the deadlock-safe sweet spot;
>   N > P puts several owners per node → >1 transfer per machine-pair, the
>   multi-stream-per-peer path §8 flags. Prefer N == P for first bring-up; if N > P
>   deadlocks, the documented next step is a `ts_demux` guest for one-transfer-per-peer.
> - **Inputs to stage on the cluster:** `TestData/mnist/{model,test}.csv`,
>   `TestData/matrix/{A,B}_<N>.bin`, `TestData/terasort/records_*.txt`.

### A. WasMem side (cluster execution via gen_variants → partition → submit)

| Workload | Cross-node DAG | `gen_variants` | Guest arg ready? | Change needed | Effort |
|----------|----------------|----------------|------------------|---------------|--------|
| **WordCount** | ✓ `word_count_auto_placement.json` | ✓ (control) | n/a (welded fan) | New `run.sh` = adapt `wc_size_sweep.sh` → emit cross-system `results.csv` cols, fixed placement (drop policy axis) | **LOW** |
| **FINRA** | ✓ `finra_auto_placement.json` | ✓ (hybrid shard `rule_*`) | ✓ packs rule/shard/n | New `run.sh` = adapt `finra_sweep.sh` | **LOW** |
| **ML training** | ✓ `ml_training_auto_placement.json` + `gen_sgd_ap_dag.py` | ✓ (`train_*`) | ✓ packs shard | New `run.sh` = adapt `ml_training_sweep.sh` | **LOW** |
| **Matrix** | ✗ **author `matrix_auto_placement.json`** | ✗ **add support** | ✓ (`mat_block` arg already packs i,j,r,c,N) | (1) author symbolic 2-D SUMMA grid DAG (`block_i_j` fan, `placement:"all"` `tile` producer + `replicate` A/B inputs); (2) add Matrix to `gen_variants` (grid placement + per-machine local-combine gather over `C_BASE+i*C+j` slots); (3) new `run.sh` | **HIGH** |
| **ML inference** | ✗ **author `ml_inference_auto_placement.json`** | ✗ **add support** | ✓ (`infer_predict` arg packs slots) | (1) author symbolic DAG — homogeneous `predict_*` fan, **like ml_training** (broadcast model via `placement:"all"` `setup_model`, shard via `partition`); (2) add `FAN_PREFIX["ml_inference"]="predict_"` + `AGGREGATOR["ml_inference"]="aggregate"` + producer co-loc on `partition`; (3) new `run.sh` | **MED** |
| **TeraSort** *(new #7)* | ✗ **author `terasort_auto_placement.json`** | ✗ **add support** | n/a — **new guest** `terasort.rs` (`ts_sample`/`ts_partition`/`ts_merge`) over the existing `Shuffle` primitive | (1) write the sort guest (additive); (2) author symbolic DAG using `Shuffle` (all-to-all) + `placement:"all"`/`fanout:N`; (3) add `gen_variants` support; (4) new `run.sh`. Folder + plan already stubbed at `../Intra-Node Application_Benchmark/TeraSort/`. | **HIGH** (new guest + the shuffle gather path) |

Common WasMem change: a folder `run.sh` shape that **drops the policy sweep axis**
(that's the Scheduling-Policy experiment) and instead fixes one placement, emitting
the cross-system column set (§4). The submit/timing/median/ground-truth machinery is
copied verbatim from the Scheduling-Policy sweeps.

### B. Baseline side (per workload, same three deployments)

Current state for **every** workload: `baseline/{rmmap,cloudburst,faasm}/` hold a
**single-box** local driver over a **local** Redis. The per-stage function logic is
reusable; the **deployment layer** is what changes.

| System | Current (intra) | Inter-node change | Effort |
|--------|-----------------|-------------------|--------|
| **RMMap** | local ES python driver, `multiprocessing`, local Redis | Deploy the same ES functions on **Knative** across the nodes (`service.yaml` flows — templates in `Benchmarks/RMMap/`); `REDIS_HOST`→box. ES protocol only, **no MITOSIS module**. | **HIGH** |
| **Cloudburst** | local Redis runner, single process | Deploy executors/schedulers on **Kubernetes** across the nodes; `REDIS_HOST`→box (Anna stays replaced by Redis). | **HIGH** |
| **Faasm** | local `wasmtime` demo driver | **Per-node launcher agent** places Faaslets on each node; state already via Redis → no transport code, just the agent + `REDIS_HOST`→box. | **MED** |

Common baseline change: `REDIS_HOST` → the dedicated Redis box (config, every
tree already reads it); keep the correctness gates; run one system at a time.

### C. Suggested order (matches the Phase plan)

1. **WordCount WasMem** (LOW) + its three baseline deployments → proves the whole
   inter-node pipeline incl. the riskiest infra (Knative/k8s/Faasm-agent standup).
2. **FINRA + ML-training WasMem** (LOW) — reuse the standing baselines.
3. **ML-inference** (MED) then **Matrix** (HIGH) — the two new-DAG workloads.
4. **TeraSort** (HIGH, new #7) — new sort guest + the all-to-all shuffle path; its
   baselines are WordCount-shaped clones (none native). Best inter-node
   differentiator (whole dataset shuffled once); do after the pipeline is proven.
5. Everything above at **N=4** first; then Phase 5 re-runs at **N=9**.
