# Inter-Node Streaming Application Benchmark — design & progress tracker

**Status: DESIGN / SCAFFOLDING (started 2026-06-19). Nothing run yet.** This is
the single source of truth for the *cluster* version of the streaming-application
track. The single-machine version is done and lives in `../intra-node/`; this
file says what changes to lift it across the cluster and how far along we are.
Update the checkboxes in [§8](#8-progress-tracker) as work lands.

---

## 1. What this experiment is (and how it differs from `../intra-node/`)

`../intra-node/` ran the two RTSFaaS-derived workloads (**MediaReview**,
**SocialNetwork**) on a **single box**: WasMem over the SHM page-chain, RTSFaaS
single-node over RoCE loopback (in-memory, no TiKV), Flink StateFun via
docker-compose (1 host). Headline (req/s, FT off for baselines): WasMem
~84k/54k, RTSFaaS ~5.4k/6.2k, StateFun ~2.4k/1.5k — see
`../intra-node/results_throughput_comparison.md`.

**This** experiment is the genuine inter-node version: same two workloads, same
three systems, but the keyed state is **partitioned across the cluster** and the
cross-partition accesses cross the network. Each system moves cross-node state
its own way — and that is the whole point:

| system | cross-node state movement (the variable under test) |
|--------|-----------------------------------------------------|
| **WasMem** (ours) | zero-copy **RDMA page-chain** — key-owning node per range; serialization-free |
| **RTSFaaS** | **RDMA lease transfer + TiKV** — its actual 5-node design point (lease-based CC, affinity-aware lease assignment) |
| **Flink StateFun** | **Kafka + protobuf + HTTP** across k8s workers; state in the (heap) backend per partition |

> Scientific question (same as intra-node, lifted up the hierarchy): when a
> keyed-state access lands on **another machine**, WasMem fetches/commits it RDMA
> zero-copy; StateFun routes it through Kafka/serialization; RTSFaaS transfers a
> lease over RDMA against TiKV-backed state. The gap should track the
> **multi-partition (cross-node) ratio** and **skew** — exactly the knobs the
> RTSFaaS paper sweeps.

This is also the first run where RTSFaaS and StateFun are at their **real design
point** (multi-node), so unlike intra-node these are not handicapped — the
comparison is the honest one for the paper.

## 2. WasMem execution (cluster: the cross-node RDMA routing variant)

Intra-node folded partition-by-key into `*_parse` (slot = 10 + key%n) with one
`*_apply` per slot on one box (`../intra-node/gen_dag.py`, `topo=intra-shm`). The
cluster version needs the **key-owning-node** routing that was the standing TODO:

- A new symbolic DAG under `DAGs/rdma_workload_dag/` (or `gen_dag.py --cluster`)
  where each key range is owned by a node, and `*_apply` for a remote key range
  is reached via the **RDMA page-chain** (`RemoteSend`/`RemoteRecv` or one-sided
  `RemoteAtomic`) instead of a local slot. This is the streaming analogue of the
  `Shuffle`/`rdma_workload_dag` path the inter-node *application* benchmark uses.
- Driven the same way as the Scheduling-Policy / inter-node app suite:
  `gen_variants → partition → submit` across the cluster (NOT `node-agent run`).
- **Multi-partition ratio** (`gen_events.py --multipartition`, already a knob but
  inert single-node) becomes the **cross-node state-access ratio** — the headline
  x-axis. **Skew** (`--skew`) → hot-node imbalance.

Reusable as-is: `../intra-node/gen_events.py` (the deterministic stream + gate),
the guests `Executor/guest/src/workloads/{media_review,social_network}.rs`, the
correctness gate. New: the cluster DAG + the key-owning RDMA routing; a
cluster `run.sh` emitting the cross-system `results.csv` columns.

## 3. How the three systems run inter-node

**WasMem** — cluster RDMA, above. State partitioned by key range across N nodes;
cross-range access = RDMA page-chain. FT via the `Persist` offload (async +
sync-barrier modes already built in `../intra-node/gen_dag.py`,
`WC_PERSIST`/`WC_PERSIST_SYNC`) — for the cluster, persist per node.

**RTSFaaS** — its native 5-node deployment (what the repo ships for). We already
have the **jar + `rtfaas:1.0` image built** and the single-node bring-up
documented (`../intra-node/baseline/RTSFaaS/single_node/`). Inter-node = revert
to the shipped multi-node config and add TiKV:
- `Env/Cluster.env`: real `driverHost` + N `workerHosts` (cluster RoCE IPs /
  reverse-DNS names — note the `getHostName()` match gotcha from single-node).
- `Env/Database.env`: `isRemoteDB=1, isTiKV=1` + stand up a **TiKV cluster**
  (TiUP, per the RTSFaaS README) — the one piece we skipped single-node.
- `isRDMA=1` across nodes (RDMA CM needs `rdma_ucm` on every box — see
  [[host-rdma-roce]]); `scripts/FaaS/run-application.sh` deploys driver/database/
  workers/client over ssh.
- Keep the `RDMABenchmark` database-role `join()` patch only if running the
  in-memory store; with TiKV the database role is replaced by TiKV.

**Flink StateFun** — k8s multi-worker. The manifests already exist
(`../intra-node/baseline/FlinkStateFun/deployment/k8s/`: kafka.yaml/topics.yaml/
flink-statefun.yaml/{mediareview,socialnetwork}-function.yaml). Inter-node =
deploy on the cluster (Strimzi Kafka), scale `statefun-worker` replicas +
`parallelism.default` across nodes, scale the function deployments. Driver =
`stream_bench.py batch` (events/s) from a producer pod. FT stance per §5.

## 4. Metrics & CSV shape

Carry over the intra-node controlled protocol (`results_common_comparison.csv`)
so figures overlay:

- **Throughput**: end-to-end **req/s = events/s**, fixed-batch max-rate.
- **Latency**: p50 / p99 (StateFun, RTSFaaS report it; WasMem cluster makespan).
- **Cross-node bytes**: RDMA bytes (ours) vs Kafka/TiKV network bytes
  (baselines) — the resource-cost axis, the inter-node headline.
- **Per-node peak memory** (incl. KV / RocksDB-or-heap / TiKV).
- Swept over **multi-partition ratio** and **skew**, plus **node count**.

`results.csv` columns (proposed): `system,workload,nodes,multipartition,skew,events,throughput_req_s,p50_ms,p99_ms,cross_node_MB,peak_mem_MB_per_node`.

## 5. Fairness rules (carried over + inter-node specifics)

- Same **unit** (events/s), same **fixed-batch max-rate** measurement, same N
  where each system can sustain it (RTSFaaS single-node capped at 16k; cluster
  should lift that — re-check).
- **Fault tolerance**: intra-node ran baselines FT-off to isolate dataflow cost.
  Inter-node, decide per the paper's claim — recommended: **FT off for all**
  (StateFun checkpointing off + heap backend; RTSFaaS in-memory or note TiKV
  durability as a labeled handicap; WasMem `Persist` off for the throughput line,
  plus a separate FT-on bar like intra-node). Keep it explicit.
- **Matched resources**: same node count + per-node core budget across systems;
  report per-node and per-core.
- One uncontrolled variable on purpose: **how cross-node state moves** (RDMA
  page-chain vs Kafka vs RDMA-lease/TiKV) — that is what we are measuring.
- RTSFaaS intrinsically runs transactional CC (can't disable) — footnote it,
  same as intra-node.

## 6. Directory layout (mirrors `../intra-node/` and the inter-node app sibling)

```
inter-node/
├── EXPERIMENT_PLAN.md            # this file
├── README.md                     # short pointer + status (TODO)
├── gen_dag.py                    # ours: cluster RDMA routing DAG (key-owning node)  (TODO)
├── run.sh                        # ours: gen_variants → partition → submit → results.csv (TODO)
├── results.csv                   # ours (cluster)                                    (TODO)
├── plot.py                       # 3-system overlay, reuse ../intra-node/plot.py style (broken axis)
├── baseline/
│   ├── RTSFaaS/                  # cluster Env + TiUP/TiKV deploy (reuse single_node/ mechanics) (TODO)
│   └── FlinkStateFun/            # k8s multi-worker deploy (manifests already exist) (TODO)
└── figs/
```

Reuse `../intra-node/gen_events.py` (shared workload), the guests, and
`../intra-node/plot.py` (broken two-stage axis, muted palette) so the inter-node
figure is visually consistent with the intra-node one.

## 7. Phase plan

0. **Cluster prep** — RDMA (`rdma_ucm`) + Docker on all nodes; Strimzi on k8s;
   TiUP/TiKV reachable. Confirm RoCE IPs + reverse-DNS names per node.
1. **Flink StateFun cluster** (lowest risk — manifests exist): deploy on k8s,
   scale workers, run `batch` for both workloads → first inter-node points.
2. **RTSFaaS cluster** (its design point): Cluster.env + TiKV via TiUP, deploy 4
   roles over the cluster, run both workloads. (Reuse the built image + the
   single-node fixes.)
3. **WasMem cluster**: author the key-owning RDMA routing DAG + cluster `run.sh`;
   correctness gate must still match `gen_events.py`; then throughput.
4. **Sweeps**: multi-partition ratio × skew × node count; cross-node bytes + mem.
5. **Figures + headline table**: overlay all three, same style as intra-node.

## 8. Progress tracker

### Infra
- [ ] `rdma_ucm` + Docker on all cluster nodes; RoCE IP/hostname map recorded.
- [ ] Strimzi Kafka operator on the cluster.
- [ ] TiKV cluster up via TiUP (for RTSFaaS design-point run).

### Flink StateFun (cluster)
- [ ] Deploy `deployment/k8s/*` on the cluster; scale workers + parallelism.
- [ ] `batch` throughput (events/s) both workloads; gate passes.

### RTSFaaS (cluster, design point)
- [ ] Cluster.env (N workers) + Database.env `isTiKV=1`; deploy over ssh.
- [ ] Both workloads at scale; client req/s + latency.

### WasMem (cluster)
- [ ] Key-owning RDMA routing DAG (`DAGs/rdma_workload_dag/` or `gen_dag.py --cluster`).
- [ ] Cluster `run.sh` (gen_variants → partition → submit → results.csv).
- [ ] Correctness gate matches `gen_events.py`; throughput + FT (Persist per node).

### Sweeps / analysis
- [ ] multi-partition ratio × skew × node count.
- [ ] cross-node bytes (RDMA vs Kafka/TiKV) + per-node peak memory.
- [ ] `plot.py` overlay + headline table (intra-node style).

## 9. References

- Single-machine version (done): `../intra-node/` — `results_throughput_comparison.md`,
  `results_common_comparison.csv`, `figs/streaming_overlay.pdf`, the FT modes
  (`results_was_ft_modes.csv`), and the baseline bring-ups under `../intra-node/baseline/`.
- RTSFaaS single-node recipe + 7 fixes: `../intra-node/baseline/RTSFaaS/single_node/README.md`.
- Host RDMA/RoCE facts (modprobe, NIC IPs): memory `[[host-rdma-roce]]`.
- Workload semantics + gate: `../intra-node/README.md`, the original RTSFaaS spec
  under `../intra-node/baseline/RTSFaaS/`.
- Inter-node *application* benchmark (conventions to mirror): `../../Inter-Node Application_Benchmark/EXPERIMENT_PLAN.md`.
