# Inter-Node Deployment — experiment plan (shared infra for both benchmarks)

**Status: DESIGN (started 2026-06-19).** This is the cross-cutting *deployment*
plan for running BOTH inter-node benchmarks on the cluster:

- **Application benchmark** — `../Inter-Node Application_Benchmark/` (WordCount,
  TeraSort, Finra, Matrix, ML_training, ML_inference; baselines RMMap, Cloudburst,
  Faasm).
- **Streaming benchmark** — `../Streaming Application_Benchmark/inter-node/`
  (MediaReview, SocialNetwork; baselines RTSFaaS, Flink StateFun).

WasMem (ours) already runs inter-node via `node-agent submit`; this plan is about
standing up **every baseline's distribution layer** on the same cluster, on equal
hardware/data. It expands the two-line `todo.md`:
1. *k8s-based frameworks → deploy Kubernetes;*
2. *non-serverless frameworks → a small per-node Python agent for collaboration.*

---

## 1. Framework inventory & deployment category

Every system across both benchmarks, grouped by HOW it distributes across nodes —
this grouping is the whole plan.

| System | Benchmark(s) | Distribution layer | Category | State / transport |
|--------|--------------|--------------------|----------|-------------------|
| **WasMem** (ours) | app + streaming | `node-agent` coordinator + workers | **(A) native agent — DONE** | RDMA page-chain (zero-copy) |
| **Cloudburst** | app | Kubernetes (executors/schedulers as pods) | **(B) Kubernetes** | Redis (in place of Anna) |
| **Flink StateFun** | streaming | Kubernetes (Strimzi Kafka + StateFun master/workers) | **(B) Kubernetes** | Kafka + protobuf + HTTP |
| **RMMap / dmerge** | app | Knative (on k8s) — Service + Sequence/Parallel flows | **(C) Knative on k8s** | Redis (ES protocol; no MITOSIS kmod w/o auth) |
| **Faasm** | app | none (clean-slate Faaslets + Faabric) | **(D) per-node Python agent** | Redis (Faasm KV) |
| **RTSFaaS** | streaming | own ssh scripts (`run-application.sh`) | **(E) ssh-orchestrated native** | RDMA leases + TiKV |

So the deployment work splits into five tracks: **(A)** done, **(B)** one k8s
cluster shared by Cloudburst + StateFun, **(C)** Knative layered on that k8s,
**(D)** the per-node Python agent (the todo's second point — Faasm + a generic
fallback), **(E)** RTSFaaS's ssh path + TiKV.

## 2. Cluster topology & shared services

```
Compute nodes (RoCE 10.10.1.x):   node0=10.10.1.2 (coordinator/k8s control-plane)
                                  node1=10.10.1.1  node2=10.10.1.3  node3=10.10.1.4
                                  → scale to 9 for the full run.
Dedicated services (off the compute path, so store traffic ≠ compute CPU/RAM):
  - Redis box   (5th machine)  — app baselines' shared KV (REDIS_HOST). One box, neutral hop.
  - Kafka       — StateFun ingress/egress (Strimzi on k8s, or the dedicated box).
  - TiKV        — RTSFaaS state store (TiUP cluster; can co-locate on the services box).
```

Record once and reuse everywhere: each compute node's RoCE IP **and** its
reverse-DNS hostname (the `getHostName()` match gotcha bit us in the streaming
run — see `[[host-rdma-roce]]`), plus the services box IP/NIC. RDMA needs
`sudo modprobe rdma_ucm` on **every** node each boot.

**Fairness invariants (both benchmarks):** identical node count + per-node core
budget across systems; FT decided once and applied to all (recommend FT-off for
the throughput line, matching intra-node); one uncontrolled variable on purpose —
how cross-node state moves (RDMA page-chain vs serialized Redis/Kafka vs RDMA-lease/TiKV).

## 3. Track (B) — the shared Kubernetes cluster

One k8s cluster spanning the 4 (→9) compute nodes serves **Cloudburst** (app) and
**Flink StateFun** (streaming); Knative (track C) layers on top for RMMap.

- [ ] Bring up k8s control-plane on node0 (10.10.1.2), join node1-3 as workers
      (kubeadm or k3s — k3s is lighter and faster to stand up; pick one, record it).
- [ ] CNI + verify pod-to-pod across nodes; label nodes so we can pin replicas.
- [ ] **Flink StateFun**: deploy the existing manifests
      `../Streaming Application_Benchmark/intra-node/baseline/FlinkStateFun/deployment/k8s/*`
      (Strimzi Kafka, flink-statefun, the 2 function deployments). Scale
      `statefun-worker` replicas + `parallelism.default` across nodes. Driver =
      `stream_bench.py batch` from a producer pod. (Most of this is built; it ran
      single-host via docker-compose — k8s is the cluster lift.)
- [ ] **Cloudburst**: executors + scheduler + (Anna→)Redis as k8s-managed pods
      across the nodes; reuse the per-stage function code from
      `../Intra-Node Application_Benchmark/<W>/baseline/cloudburst/`. Point its KV
      at the dedicated Redis box.

## 4. Track (C) — Knative (RMMap) on the k8s cluster

- [ ] Install Knative Serving on the cluster (on top of track B's k8s).
- [ ] Express each app workload as a Knative `Service` + `Sequence`/`Parallel`
      flow across nodes; reuse `../Intra-Node Application_Benchmark/<W>/baseline/rmmap/`
      stage code. State via Redis (ES protocol). **No MITOSIS kernel module**
      without explicit authorization — use the Redis path.

> ⚠️ **Warm-reuse vs. pure cold-start — measure BOTH, label every result.** The
> default RMMap manifest pins `min-scale=1` (1 warm pod per stage → warm-reuse, no
> startup cost). We also run a **strict cold-start** case (`min-scale=0` +
> scale-to-zero, 0 pods before each measured request) to capture pod
> scheduling + container + app init latency. These are different numbers — never mix
> them in one table; tag series `*-warm` / `*-cold`. Exact config + procedure:
> [`k8s/AUTOSCALING.md`](k8s/AUTOSCALING.md) §5.

### Cross-system warm/cold definitions

Two named experiment modes apply to **every** baseline. Tag every result row with its mode.

| Mode | Description | Trigger between runs |
|------|-------------|----------------------|
| **Normal (warm-reuse)** | Pods/roles/Faaslets stay alive after a run and are reused by the next. Each framework's own keep-warm timeout governs teardown. Measures steady-state throughput/latency with **no startup cost**. | Do nothing — let the framework manage lifecycle. |
| **Full cold-start** | After each run, **terminate all pods/roles/Faaslets** before the next invocation. Measures end-to-end latency **including** scheduling + container/process + app-init cost. | Actively kill all roles; confirm zero before timing the next request. |

Per-baseline "cold" procedure:

| System | Normal (warm) | Full cold-start |
|--------|---------------|-----------------|
| **RMMap (Knative)** | `min-scale=1` keeps 1 pod/stage alive indefinitely. | Set `min-scale=0`, wait for scale-to-zero (~90 s) or delete pods manually; confirm 0 pods before each run. See `k8s/AUTOSCALING.md` §5. |
| **Cloudburst (k8s)** | Executor + scheduler pods stay scheduled; k8s restarts if they crash. | `kubectl delete pods -l app=cloudburst-executor` after each run; wait for pod count → 0 before next submit. |
| **Faasm (per-node agent)** | Faaslet processes stay in the agent's process pool between runs (the agent itself stays up). | After each run, send `POST /stop` for every Faaslet handle and confirm all handles show exited; the agent daemon stays up but no Faaslets are running before the next invocation. |
| **RTSFaaS** | Role containers (driver/database/worker/client) stay up following RTSFaaS's internal keep-warm timeout. | Kill all role containers on every node (`sudo docker rm -f $(sudo docker ps -q)` or the agent's `/stop`); confirm 0 role containers before starting the next run. |
| **Flink StateFun (k8s)** | StateFun master + worker pods stay scheduled. | Scale `statefun-worker` deployment to 0, wait, then scale back up before the next run. |
| **WasMem (node-agent)** | `node-agent` stays running; WASM modules are submitted per-job (AOT pre-loaded, JIT compiles on first invocation). Keep-warm is not applicable — the agent is always up. | For a cold-module baseline: unload module cache between runs (if supported) or compare JIT-first-invocation vs. AOT-pre-loaded. Otherwise cold-start is not meaningful for WasMem (no container/pod scheduling overhead). |

## 5. Track (D) — the per-node Python agent (todo point #2)

For systems with **no orchestration framework** (Faasm is clean-slate) — and as a
**generic fallback runner** for any baseline we can't stand up on its real
framework in time — we run a tiny Python agent on every node, the baseline-side
analogue of WasMem's `node-agent`.

**Design (keep it minimal):**
- `agent.py` on each compute node: a small HTTP (Flask/`http.server`) or ZMQ
  server exposing:
  - `POST /launch {cmd, env, cwd}` → start a local process (a Faaslet runner, a
    baseline worker binary, …), return a handle.
  - `GET /status/{handle}` / `POST /stop/{handle}` → lifecycle + exit code + stderr tail.
  - `POST /stage {path, bytes}` and `GET /file/{path}` → push/pull inputs &
    result files (so we don't assume a shared FS — the streaming run showed the
    worker needs inputs staged, not just NFS-present).
  - `GET /metrics` → cpu%, rss, per-process — for the per-node memory/throughput sampler.
- `coordinator.py` on node0: reads a job spec `{system, workload, placement:
  node→[functions], redis_host, args}`, calls each node's agent to launch its
  share, triggers the run, polls status, collects timings + result files, writes
  `results.csv` in the same column shape as the WasMem/k8s runs.
- **Faasm specifics:** the agent launches Faaslets on its node (the per-node
  placement Knative/k8s do for the others); Faasm KV → the dedicated Redis box.
- Reuse, don't fork: the agent just *launches* the existing baseline stage code
  under `../Intra-Node Application_Benchmark/<W>/baseline/faasm/`; placement +
  Redis wiring is the only new logic.

**Checklist:**
- [ ] `agent.py` (launch/status/stop/stage/metrics) + `coordinator.py` (placement + collect).
- [ ] systemd/`tmux`/`ssh` bring-up of the agent on all nodes; health check.
- [ ] Faasm runner integration (launch Faaslets, point KV at Redis box).
- [ ] Emit `results.csv` (shared column shape) + per-node memory samples.

## 6. Track (E) — RTSFaaS (ssh-orchestrated) + TiKV

RTSFaaS ships its own multi-node path; reuse it rather than re-wrapping.
- [ ] TiKV cluster up via **TiUP** (per the RTSFaaS README) on the services box.
- [ ] `Env/Cluster.env`: real driver + N worker hosts (cluster RoCE reverse-DNS
      names); `Env/Database.env`: `isRemoteDB=1, isTiKV=1`; `isRDMA=1`
      (`rdma_ucm` on every node).
- [ ] Deploy the 4 roles via `scripts/FaaS/run-application.sh` (ssh). Reuse the
      built `rtfaas:1.0` image + the single-node fixes documented in
      `../Streaming Application_Benchmark/intra-node/baseline/RTSFaaS/single_node/README.md`.
- [x] Needs passwordless ssh between nodes for this account (the streaming run
      hit `Permission denied (publickey)` to node1 — set up keys first).
      **Done:** RSA key `wasmem-node0-rsa` (`~/.ssh/id_rsa`) generated on node-0
      and added to the CloudLab profile; Emulab propagates it to all nodes.
      Use `ssh node-1` … `ssh node-8` from node-0 (hostnames resolved via `/etc/hosts`).

## 7. Phase plan (deploy order = lowest risk first)

0. **Cluster prep (shared):** RDMA `rdma_ucm` all nodes; passwordless ssh ✅ (`~/.ssh/id_rsa`, RSA key added to CloudLab profile — `ssh node-1` … `ssh node-8` works from node-0); record
   IP/hostname map + services-box IP. Stand up the **dedicated Redis box**.
1. **WasMem** — already running; lock in the run recipe + `results.csv` shape both benchmarks. ✅
2. **k8s cluster (track B)** — control-plane + workers; smoke test.
3. **Flink StateFun on k8s** — manifests exist; first streaming baseline inter-node.
4. **per-node Python agent (track D)** + **Faasm** — first app baseline inter-node;
   this agent also de-risks every other baseline as a fallback.
5. **Cloudburst on k8s** (track B) and **RMMap on Knative** (track C).
6. **RTSFaaS + TiKV (track E)** — its design-point streaming run.
7. **Sweeps + figures** — per benchmark, overlay with the intra-node figures
   (same StateSync palette / column shape).

## 8. Risks & fallbacks

- **k8s/Knative/Cloudburst bring-up is the historical blocker** — the intra-node
  suite *abstracted* RMMap/Cloudburst because the real stacks were hard on one
  host (see `../Intra-Node Application_Benchmark/EXPERIMENT_RUNBOOK.md §4`). On the
  cluster we attempt the real frameworks; if one can't be stood up in time, fall
  back to the **track-D per-node agent runner** for that system, **with the gap
  documented** (not silently). This is why the agent is worth building early.
- **Shared FS not guaranteed** — the streaming run proved workers need inputs
  *staged*, not assumed NFS-present; the agent's `/stage` endpoint + StateFun's
  `shared_inputs` + k8s configmaps/PVCs each handle this per track.
- **RDMA prerequisites** — `rdma_ucm` per boot; reverse-DNS hostname matching;
  passwordless ssh for RTSFaaS. Bake into the cluster-prep checklist.
- **Coordinator liveness** — if a coordinator/daemon dies, submits fail
  ("Connection refused"); add health checks + restart hooks.

## 9. Deliverable layout

```
Tests/Inter-Node deployment/
├── EXPERIMENT_PLAN.md      # this file
├── todo.md                 # concise actionable checklist (seed of this plan)
├── agent/                  # track D: agent.py + coordinator.py (per-node runner)   (TODO)
├── k8s/                    # track B/C: control-plane bring-up + shared manifests   (TODO)
│   ├── cloudburst/         # reuse intra baseline stage code + k8s deploy
│   └── knative/            # RMMap flows
├── redis/                  # dedicated Redis box bring-up + REDIS_HOST record       (TODO)
└── README.md               # cluster IP/hostname map + per-track quickstart         (TODO)
```
Per-benchmark *run logic* stays in each benchmark's `inter-node/` folder
(`../Inter-Node Application_Benchmark/`, `../Streaming Application_Benchmark/inter-node/`);
THIS folder is the shared *deployment* substrate they call into.

## 10. References

- App-benchmark inter-node design (baselines, Redis box, fairness): `../Inter-Node Application_Benchmark/EXPERIMENT_PLAN.md` (§3, §6).
- Streaming inter-node (WasMem cluster done; RTSFaaS/StateFun bring-up): `../Streaming Application_Benchmark/inter-node/{EXPERIMENT_PLAN,todo}.md`.
- StateFun k8s manifests + RTSFaaS single-node recipe: `../Streaming Application_Benchmark/intra-node/baseline/{FlinkStateFun/deployment/k8s,RTSFaaS/single_node}/`.
- Intra baseline stage code (reused, unchanged) + why frameworks were abstracted: `../Intra-Node Application_Benchmark/<W>/baseline/{cloudburst,rmmap,faasm}/`, `EXPERIMENT_RUNBOOK.md §4`.
- Host RDMA/RoCE facts: memory `[[host-rdma-roce]]`.
