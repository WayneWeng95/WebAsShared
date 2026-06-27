#!/usr/bin/env bash
# run.sh — deploy the RMMap mapper ksvc (warm|cold) and drive the WordCount run.
# Resolves the Kourier gateway + ksvc Host header from the cluster, then runs driver.py.
#
#   ./run.sh warm 15      # pre-warmed 60-pod fan, 15 reps  (headline RMMap bar)
#   ./run.sh cold 5       # strict scale-to-zero cold start, 5 reps
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
VARIANT="${1:-warm}"
REPS="${2:-15}"
NS=baselines
CORPUS="${CORPUS:-/opt/myapp/WebAsShared/TestData/corpus_4gb.txt}"
CSV="${CSV:-/opt/myapp/WebAsShared/Tests/Inter-Node Application_Benchmark/rmmap/results_wordcount.csv}"
REDIS_HOST="${REDIS_HOST:-10.10.1.2}"
REDIS_PORT="${REDIS_PORT:-30679}"

# free the executor pool's node capacity for the 60 warm mapper pods (reversible).
kubectl -n "$NS" scale deploy/cloudburst-executor --replicas=0 >/dev/null 2>&1 || true

kubectl apply -f "$HERE/service-$VARIANT.yaml"
kubectl -n "$NS" wait --for=condition=Ready ksvc/wc-mapper --timeout=300s

HOST="$(kubectl -n "$NS" get ksvc wc-mapper -o jsonpath='{.status.url}' | sed 's#^https\?://##')"
# Kourier external LB IP on this node's subnet (node 0 is on the subnet, off the mesh).
GW_IP="$(kubectl -n kourier-system get svc kourier -o jsonpath='{.status.loadBalancer.ingress[0].ip}')"
GATEWAY="http://${GW_IP}"
echo "[run] variant=$VARIANT reps=$REPS host=$HOST gateway=$GATEWAY"

EXTRA=()
[ "$VARIANT" = "cold" ] && EXTRA+=(--ensure-zero)

python3 "$HERE/driver.py" --corpus "$CORPUS" --fanout 60 --reps "$REPS" \
  --gateway "$GATEWAY" --host "$HOST" \
  --redis-host "$REDIS_HOST" --redis-port "$REDIS_PORT" \
  --variant "$VARIANT" --csv "$CSV" "${EXTRA[@]}"
