#!/usr/bin/env bash
# 03-install-knative.sh — install Knative Serving + Kourier + Eventing on k3s.
#
# Run on node 0 AFTER all workers have joined (02). This is RMMap's full runtime
# (track C): each stage is a Knative `Service` (Serving), and RMMap's inter-stage
# flow posts CloudEvents to the Knative **Eventing** default broker
# (`broker-ingress.knative-eventing…` — see the RMMap app/functions.py), so we
# install Serving + Kourier ingress AND Eventing + an in-memory broker.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
export KUBECONFIG="$KUBECONFIG_PATH"
log() { printf '\033[1;36m[knative]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[knative] %s\033[0m\n' "$*" >&2; exit 1; }

command -v kubectl >/dev/null 2>&1 || alias kubectl="k3s kubectl"
kubectl get nodes >/dev/null 2>&1 || die "kubectl can't reach the cluster — run 01 first / check KUBECONFIG=$KUBECONFIG_PATH"

SREL="https://github.com/knative/serving/releases/download/knative-${KNATIVE_VERSION}"
KREL="https://github.com/knative/net-kourier/releases/download/knative-${KOURIER_VERSION}"
EREL="https://github.com/knative/eventing/releases/download/knative-${KNATIVE_VERSION}"

log "1/7 Serving CRDs ($KNATIVE_VERSION)"
kubectl apply -f "$SREL/serving-crds.yaml"                || die "CRDs apply failed (offline? see README)"
log "2/7 Serving core"
kubectl apply -f "$SREL/serving-core.yaml"                || die "core apply failed"
log "3/7 Kourier networking ($KOURIER_VERSION)"
kubectl apply -f "$KREL/kourier.yaml"                     || die "kourier apply failed"

log "4/7 select Kourier as the Serving ingress"
kubectl patch configmap/config-network -n knative-serving --type merge \
  -p '{"data":{"ingress-class":"kourier.ingress.networking.knative.dev"}}'  || die "ingress-class patch failed"

log "5/7 magic DNS (sslip.io) so Services get a real URL"
kubectl apply -f "$SREL/serving-default-domain.yaml"      || log "WARN: default-domain job failed; set a domain manually (README)"

log "6/7 Eventing (RMMap's inter-stage CloudEvent broker)"
kubectl apply -f "$EREL/eventing-crds.yaml"              || die "eventing CRDs apply failed"
kubectl apply -f "$EREL/eventing-core.yaml"             || die "eventing core apply failed"
# In-memory channel + MT broker (lightweight; fine for the data-path-only boundary).
kubectl apply -f "$EREL/in-memory-channel.yaml"         || die "in-memory-channel apply failed"
kubectl apply -f "$EREL/mt-channel-broker.yaml"         || die "mt-channel-broker apply failed"

log "7/7 wait for Serving + Kourier + Eventing to roll out …"
kubectl -n knative-serving  rollout status deploy/controller --timeout=180s || die "serving controller not ready"
kubectl -n knative-serving  rollout status deploy/webhook    --timeout=180s || die "serving webhook not ready"
kubectl -n kourier-system   rollout status deploy/3scale-kourier-gateway --timeout=180s || die "kourier gw not ready"
kubectl -n knative-eventing rollout status deploy/eventing-controller --timeout=180s || die "eventing controller not ready"
kubectl -n knative-eventing rollout status deploy/mt-broker-ingress   --timeout=180s || die "broker ingress not ready"

# RMMap posts to the "default-broker" in namespace "default" — create it.
kubectl create namespace default >/dev/null 2>&1 || true
cat <<'YAML' | kubectl apply -f - || log "WARN: default broker create failed"
apiVersion: eventing.knative.dev/v1
kind: Broker
metadata: { name: default-broker, namespace: default }
spec: {}
YAML

# k3s servicelb gives the Kourier LoadBalancer an external IP = a node IP.
KIP="$(kubectl -n kourier-system get svc kourier -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null)"
log "DONE. Serving (Kourier ${KIP:-<pending>}) + Eventing (broker-ingress.knative-eventing) up."
echo ""
echo "Smoke-test it:   $HERE/04-verify.sh"
echo "Deploy RMMap flows into:   $HERE/knative/   (reuse Intra baseline stage code; REDIS_HOST=$REDIS_HOST)"
