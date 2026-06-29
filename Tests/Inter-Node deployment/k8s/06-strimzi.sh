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
# The old strimzi.io/install/<ver>?namespace=<ns> redirector is dead (404 for version
# numbers; 'latest' now points at a 1.0.x asset). Pull the canonical GitHub release YAML
# and do the namespace rewrite ourselves (what the redirector used to do): every
# `namespace: myproject` → our ns, so the operator watches $KAFKA_NS.
STRIMZI_YAML="https://github.com/strimzi/strimzi-kafka-operator/releases/download/${STRIMZI_VERSION}/strimzi-cluster-operator-${STRIMZI_VERSION}.yaml"
# Bump the operator's memory from the upstream default (384Mi) to 1Gi: on this
# 9-node cluster the operator OOMKills (exit 137) at 384Mi mid-reconciliation and
# crashloops, so new KafkaTopic CRs never get created for StateFun.
strimzi_manifest="$(curl -fsSL "$STRIMZI_YAML" | sed "s/namespace: .*/namespace: ${KAFKA_NS}/; s/memory: 384Mi/memory: 1Gi/g")" \
  || die "Strimzi YAML fetch failed ($STRIMZI_YAML) — offline? mirror it (see README)"
printf '%s\n' "$strimzi_manifest" | kubectl create -n "$KAFKA_NS" -f - 2>/dev/null \
  || printf '%s\n' "$strimzi_manifest" | kubectl apply -n "$KAFKA_NS" -f - \
  || die "Strimzi operator install failed"
kubectl -n "$KAFKA_NS" rollout status deploy/strimzi-cluster-operator --timeout=180s || die "operator not ready"

log "2/3 apply Kafka cluster + topics (statefun/statefun-kafka.yaml)"
kubectl apply -n "$KAFKA_NS" -f "$HERE/statefun/statefun-kafka.yaml" || die "Kafka CR apply failed"

log "3/3 wait for the Kafka cluster to be Ready (brokers + zookeeper + entity-operator) …"
kubectl -n "$KAFKA_NS" wait kafka/kafka-cluster --for=condition=Ready --timeout=420s \
  || die "Kafka not Ready (kubectl -n $KAFKA_NS get kafka,pods); ephemeral storage + 2 brokers need a few min"

log "DONE. bootstrap (in-cluster): kafka-cluster-kafka-bootstrap.$KAFKA_NS.svc:9092"
kubectl -n "$KAFKA_NS" get kafkatopic -o name | sed 's#^#  #'
echo "Next: 07-load-images.sh (StateFun images), then apply the StateFun master/function deployments."
