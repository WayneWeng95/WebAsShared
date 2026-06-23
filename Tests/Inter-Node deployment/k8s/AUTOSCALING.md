# Knative autoscaling — warm instances & keep-alive (current setup)

How the k8s/Knative baselines (RMMap on Knative) behave with respect to **warm
instances** (min replicas) and **keep-alive** (how long an idle pod survives before
scale-to-zero). Captured from the live cluster + the manifests in this repo.

There are **two layers**: the cluster-wide autoscaler defaults, and per-Service
annotation overrides. The per-Service overrides are what actually governs RMMap.

---

## 1. Cluster-wide defaults — `config-autoscaler` (UNTOUCHED)

`03-install-knative.sh` does **not** patch the `config-autoscaler` ConfigMap. Live,
its `data` holds only the `_example` block (no active overrides), so Knative runs on
its built-in defaults:

| Knob | Value | Meaning |
|------|-------|---------|
| `min-scale` | `0` | default = scale to zero (**no warm instance**) |
| `initial-scale` | `1` | a new revision starts at 1 pod |
| `max-scale` | `0` | unlimited |
| `enable-scale-to-zero` | `true` | idle revisions are removed |
| `stable-window` | `60s` | idle period before scale-down / scale-to-zero begins |
| `scale-to-zero-grace-period` | `30s` | upper bound to drain the last pod afterwards |
| `scale-to-zero-pod-retention-period` | `0s` | no extra lingering of the last pod |
| `scale-down-delay` | `0s` | no scale-down hysteresis |
| `container-concurrency-target-default` × `target-percentage` | `100 × 70%` ≈ **70 concurrent/pod** | soft scaling target |
| `requests-per-second-target-default` | `200` | RPS target if RPS metric is used |

**A service left at defaults:** 0 warm → scales up on traffic → after **~60 s idle
→ ~30 s drain → 0 pods** (cold start on the next request).

Inspect live:
```bash
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
sudo k3s kubectl -n knative-serving get configmap config-autoscaler -o jsonpath='{.data}'
```

---

## 2. Per-Service overrides — what RMMap actually uses

RMMap defines one Knative `Service` per stage. The existing template,
`Benchmarks/RMMap/digital-minist/service.yaml` (10 stages: `splitter`,
`predict-0…`, merge, …), sets:

| Annotation | Where | Effect |
|------------|-------|--------|
| `autoscaling.knative.dev/min-scale: "1"` | **all 10 stages** | **1 warm instance per stage, always on** — never scales to zero |
| `autoscaling.knative.dev/max-scale: "1"` | **`splitter` only** | pinned to exactly 1 pod (no scale-up) |

No per-Service `containerConcurrency`, `target`, `scale-down-delay`, or
`scale-to-zero-pod-retention-period` — those fall back to the cluster defaults above.

### Net effect for RMMap
- **Warm instances:** 1 warm pod per stage, so **no cold-start penalty** between
  requests. `splitter` is hard-pinned at exactly 1; the downstream
  `predict-*` / merge stages float between **1 and ∞** (default `max-scale=0`).
- **Keep-alive:** because `min-scale=1`, those pods stay alive **indefinitely** —
  there is effectively no idle teardown. The `60s` / `30s` scale-to-zero timing only
  applies to a service running at the default `min-scale=0`.

---

## 3. Caveats

1. **The inter-node `knative/` track has no Services deployed yet.** `knative/README.md`
   still says *"TODO: per-stage service.yaml + the Sequence/Parallel flow"*. The
   `min-scale: "1"` manifest lives under `Benchmarks/RMMap/digital-minist/`, not in
   `Tests/Inter-Node deployment/k8s/knative/`. When you lift it inter-node, the
   warm/keep-alive policy comes from whatever you copy there — re-confirm it then.
2. **Fairness vs. the other baselines.** `min-scale=1` runs RMMap fully warm, which
   matches RTSFaaS/Faasm keeping their roles up for a run (consistent), but it
   sidesteps Knative scale-to-zero entirely. To measure cold-start, drop `min-scale`
   (or set `allow-zero-initial-scale`) per stage.

---

## 4. How to change the policy

**Per Service** (preferred — scoped, lives with the app):
```yaml
metadata:
  annotations:
    autoscaling.knative.dev/min-scale: "1"     # warm instances (0 = scale to zero)
    autoscaling.knative.dev/max-scale: "1"     # cap (0 = unlimited)
    autoscaling.knative.dev/target: "70"       # concurrency target per pod
```

**Cluster-wide keep-alive** (affects only services NOT pinned with `min-scale≥1`):
```bash
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
sudo k3s kubectl -n knative-serving patch configmap/config-autoscaler --type merge -p \
  '{"data":{"scale-to-zero-pod-retention-period":"5m","stable-window":"60s","scale-down-delay":"0s"}}'
```
To make this reproducible, add the same `kubectl patch` to `03-install-knative.sh`
(after the Serving core apply) so every cluster comes up with the chosen policy.

---

## 5. ⚠️ Experiment modes — warm-reuse vs. pure cold-start (MEASURE BOTH)

> **Be explicit about which mode every k8s/Knative result was taken in.** Warm-reuse
> and cold-start are different numbers and must not be mixed in one table or chart.
> The plan tests **both**: the default (warm) setting AND a strict cold-start case.

| Mode | What it measures | Knative config |
|------|------------------|----------------|
| **Warm-reuse (default)** | steady-state throughput/latency, pod already running — **no** startup cost | per-Service `min-scale: "1"` (the current RMMap manifest). Warm up first, then measure. |
| **Pure cold-start** | end-to-end latency *including* pod scheduling + container + app init | `min-scale: "0"` (scale-to-zero); ensure the revision is at **0 pods** before each measured request. |

### Running the pure cold-start case
1. Remove the warm pin so the stage can reach zero — per Service:
   ```yaml
   metadata:
     annotations:
       autoscaling.knative.dev/min-scale: "0"   # was "1"
   ```
   (and drop `max-scale: "1"` on `splitter` so it isn't held at one).
2. Make teardown immediate and verifiable (cluster-wide):
   ```bash
   export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
   sudo k3s kubectl -n knative-serving patch configmap/config-autoscaler --type merge -p \
     '{"data":{"scale-to-zero-pod-retention-period":"0s","scale-down-delay":"0s","stable-window":"60s"}}'
   ```
3. Per measured invocation: confirm **0 pods**, then send one request and time it
   end-to-end (the first request goes through the Activator and pays the cold start):
   ```bash
   sudo k3s kubectl get pods -l serving.knative.dev/service=<svc>   # expect: none
   # …send request, record end-to-end latency…
   ```
   Either wait `stable-window + scale-to-zero-grace` (~90 s) between requests, or
   delete the revision's pods to force zero before the next sample.

### Reporting
- Tag every row/series with its mode (e.g. `rmmap-warm`, `rmmap-cold`).
- Cold-start applies cluster-wide: the **other baselines** keep roles warm for a run
  (RTSFaaS roles, Faasm Faaslets, WasMem node-agent), so a fair cold-start comparison
  means each baseline also starts from its cold state — note per-baseline what "cold"
  means, since only Knative natively scales to zero.
