# Cloudburst on Kubernetes (track B)

Deploy Cloudburst executors + scheduler as Deployments/pods across the k3s nodes
(set up by `../`), KV pointed at `REDIS_HOST` (cluster.env). Reuse the per-stage
function code from `../../../Intra-Node Application_Benchmark/<W>/baseline/cloudburst/`.
Measurement boundary = data-path only (EXPERIMENT_PLAN §5.9).

TODO: executor/scheduler Deployments + Service; per-workload job manifests.
