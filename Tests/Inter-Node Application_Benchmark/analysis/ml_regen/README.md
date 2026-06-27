# ML re-run on the regenerated data/model — Faasm & WasMem vs the old results

Re-ran the two ML workloads through **Faasm AND WasMem** on the SAME regenerated inputs the
Cloudburst / RMMap inter-node bars used (`ml_training_6m.csv`, `ml_inference_6m.csv`,
`ml_inference_model.csv`), to check whether the gates line up across all four systems.
6M samples, 60 workers, 4 nodes, 3 reps.

## Old vs new (6M)

**ML training** — makespan / total_job / `weight_checksum`:
| system | old (orig data) | new (regen data) |
|---|---|---|
| Faasm | 4748 / 49774 / **1232** | 5016 / 52746 / **1232** |
| WasMem | 5258 / 16386 / **1232** | 5317 / 17385 / **1232** |
| Cloudburst | — | 5541 / 139555 / **1232** |
| RMMap-RDMA | — | 3266 / 120050 / **1232** |

**ML inference** — makespan / total_job / `prediction_checksum`:
| system | old (orig model) | new (regen model) |
|---|---|---|
| Faasm | 3092 / 46550 / **18,623,474** | 3322 / 48533 / **18,633,154** |
| WasMem | 3484 / 12269 / **18,623,474** | 3746 / 14219 / **18,633,154** |
| Cloudburst | — | 2538 / 74338 / **18,633,154** |
| RMMap-RDMA | — | 1983 / 47388 / **18,633,154** |

## What changed, what didn't
- **Training gate unchanged (1232)** — the regenerated training data is deterministic and the
  integer SGD lands on the same checksum. Both Faasm and WasMem re-runs confirm it; all four
  systems agree.
- **Inference gate shifted: 18,623,474 → 18,633,154 (+9,680).** This is *purely the
  regenerated model* (`ml_inference_model.csv`, 332 B) — the original `infer_model.txt` the
  old Faasm/WasMem rows used was emptied. Same forward pass, different model weights →
  different predictions → different Σ-of-labels. **After the re-run all four systems agree on
  18,633,154** — the split was a data-version artifact, not an algorithm difference.
- **Timing essentially unchanged** old→new (Faasm train 4748→5016, infer 3092→3322; WasMem
  train 5258→5317, infer 3484→3746 — all within run-to-run noise). Same data size; the model
  regeneration doesn't affect time. (One transient: the very first Faasm training re-run read
  9046 ms — cold freshly-recompiled cwasm + a just-started host Redis — and settled to ~5000
  on the warm run.)

## Cross-system picture (regen, total execution time = total_job)
WasMem stays lowest on total_job (train 17.4 s, infer 14.2 s) — the RDMA shared-memory
substrate; the KVS/Python bars pay serialize + Python parse. accuracy 74.3% everywhere.

## Infra notes (what it took)
- **Faasm:** started a host Redis at `10.10.1.2:6379` + the node-0 agent; the precompiled
  `sgd_grad.cwasm`/`infer_predict.cwasm` were stale (wasmtime version drift → "failed
  deserialization") and were recompiled **per node** with each node's own wasmtime.
- **WasMem:** brought the cluster online with `node_update.sh` on every node (git pull +
  `build.sh` rebuilds the `host` binary and `guest.cwasm` in lockstep — the guest `.cwasm`
  must come from the host's own `host compile`, not the wasmtime CLI) + `./node-agent start
  --config NodeAgent/agent_coordinator.toml` on node 0. The inference model path was repointed
  from the (deleted) `TestData/ml/infer_model.txt` to `TestData/ml_inference_model.csv` in
  `scripts/gen_inference_ap_dag.py` (`DEFAULT_MODEL`).

Files here: `{faasm,wasmem}_ml_{training,inference}_regen.csv` (the new rows).
