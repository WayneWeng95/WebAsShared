#!/usr/bin/env bash
# 05-redis.sh — OPTIONAL: deploy a throwaway Redis in-cluster for a quick baseline
# smoke test. For real measurements use the DEDICATED 5th box (set REDIS_HOST in
# cluster.env) so KV traffic stays off the compute path (fairness rule §8) — this
# in-cluster Redis steals CPU/RAM from a worker and skews per-node memory sampling.
#
# Deploys Redis to the `baselines` namespace, exposed as a NodePort so both
# Cloudburst pods (in-cluster) and the RMMap stage code can reach it at
# <any-node-ip>:30679.  Run on node 0.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
export KUBECONFIG="$KUBECONFIG_PATH"
command -v kubectl >/dev/null 2>&1 || alias kubectl="k3s kubectl"

kubectl create namespace baselines --dry-run=client -o yaml | kubectl apply -f -
cat <<'YAML' | kubectl apply -f -
apiVersion: apps/v1
kind: Deployment
metadata: { name: redis, namespace: baselines }
spec:
  replicas: 1
  selector: { matchLabels: { app: redis } }
  template:
    metadata: { labels: { app: redis } }
    spec:
      containers:
        - name: redis
          image: redis:7-alpine
          args: ["--save", "", "--appendonly", "no"]   # in-memory, no persistence
          ports: [ { containerPort: 6379 } ]
---
apiVersion: v1
kind: Service
metadata: { name: redis, namespace: baselines }
spec:
  type: NodePort
  selector: { app: redis }
  ports:
    - port: 6379
      targetPort: 6379
      nodePort: 30679
YAML
kubectl -n baselines rollout status deploy/redis --timeout=120s
echo ""
echo "Redis up.  In-cluster:  redis.baselines.svc.cluster.local:6379"
echo "           From a node: $SERVER_IP:30679   (REDIS_HOST=$SERVER_IP REDIS_PORT=30679)"
