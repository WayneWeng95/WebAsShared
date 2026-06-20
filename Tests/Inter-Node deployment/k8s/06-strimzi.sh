#!/usr/bin/env bash
# 06-strimzi.sh — install the Strimzi Kafka operator and the StateFun Kafka env.
#
# Run on node 0 after k8s is up (01). Flink StateFun (streaming baseline) talks
# protobuf over Kafka; Strimzi is the operator that turns the Kafka/KafkaTopic CRs
# in statefun/statefun-kafka.yaml into a running broker + topics. Cloudburst/RMMap
# don't need this (they use Redis) — it's StateFun-only.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
export KUBECONFIG="$KUBECONFIG_PATH"
log() { printf '\033[1;36m[strimzi]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[strimzi] %s\033[0m\n' "$*" >&2; exit 1; }
command -v kubectl >/dev/null 2>&1 || alias kubectl="k3s kubectl"
kubectl get nodes >/dev/null 2>&1 || die "cluster unreachable — run 01 first"

log "1/3 install Strimzi operator $STRIMZI_VERSION (watching ns '$KAFKA_NS')"
# strimzi.io/install/<ver>?namespace=<ns> renders the operator to watch that ns.
kubectl create -f "https://strimzi.io/install/${STRIMZI_VERSION}?namespace=${KAFKA_NS}" -n "$KAFKA_NS" 2>/dev/null \
  || kubectl apply -f "https://strimzi.io/install/${STRIMZI_VERSION}?namespace=${KAFKA_NS}" -n "$KAFKA_NS" \
  || die "Strimzi operator install failed (offline? mirror the install YAML — see README)"
kubectl -n "$KAFKA_NS" rollout status deploy/strimzi-cluster-operator --timeout=180s || die "operator not ready"

log "2/3 apply Kafka cluster + topics (statefun/statefun-kafka.yaml)"
kubectl apply -n "$KAFKA_NS" -f "$HERE/statefun/statefun-kafka.yaml" || die "Kafka CR apply failed"

log "3/3 wait for the Kafka cluster to be Ready (brokers + zookeeper + entity-operator) …"
kubectl -n "$KAFKA_NS" wait kafka/kafka-cluster --for=condition=Ready --timeout=420s \
  || die "Kafka not Ready (kubectl -n $KAFKA_NS get kafka,pods); ephemeral storage + 2 brokers need a few min"

log "DONE. bootstrap (in-cluster): kafka-cluster-kafka-bootstrap.$KAFKA_NS.svc:9092"
kubectl -n "$KAFKA_NS" get kafkatopic -o name | sed 's#^#  #'
echo "Next: 07-load-images.sh (StateFun images), then apply the StateFun master/function deployments."
