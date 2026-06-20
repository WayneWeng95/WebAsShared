#!/usr/bin/env bash
# 04-verify.sh — smoke-test the cluster: nodes Ready, cross-node pod networking,
# and a Knative hello-world that scales from zero and answers over Kourier.
# Run on node 0. Cleans up its test objects on exit.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
export KUBECONFIG="$KUBECONFIG_PATH"
log() { printf '\033[1;36m[verify]\033[0m %s\n' "$*"; }
ok()  { printf '  \033[1;32m✓\033[0m %s\n' "$*"; }
bad() { printf '  \033[1;31m✗ %s\033[0m\n' "$*"; FAIL=1; }
FAIL=0
command -v kubectl >/dev/null 2>&1 || alias kubectl="k3s kubectl"
cleanup() { kubectl delete ksvc helloworld --ignore-not-found >/dev/null 2>&1; kubectl delete pod nettest-a nettest-b --ignore-not-found >/dev/null 2>&1; }
trap cleanup EXIT

log "1) all nodes Ready"
kubectl get nodes -o wide
want=$(( 1 + $(echo $AGENT_IPS | wc -w) ))
ready=$(kubectl get nodes --no-headers 2>/dev/null | grep -cw Ready)
[ "$ready" -ge "$want" ] && ok "$ready/$want nodes Ready" || bad "only $ready/$want nodes Ready (workers joined? run 02 on each)"

log "2) cross-node pod-to-pod networking (CNI)"
# Pin two pods to two different nodes and ping across the pod network.
n0=$(kubectl get nodes --no-headers -o custom-columns=N:.metadata.name | sed -n 1p)
n1=$(kubectl get nodes --no-headers -o custom-columns=N:.metadata.name | sed -n 2p)
kubectl run nettest-a --image=busybox --restart=Never --overrides="{\"spec\":{\"nodeName\":\"$n0\"}}" -- sleep 600 >/dev/null 2>&1
kubectl run nettest-b --image=busybox --restart=Never --overrides="{\"spec\":{\"nodeName\":\"$n1\"}}" -- sleep 600 >/dev/null 2>&1
kubectl wait --for=condition=Ready pod/nettest-a pod/nettest-b --timeout=90s >/dev/null 2>&1 || bad "test pods didn't start"
ipb=$(kubectl get pod nettest-b -o jsonpath='{.status.podIP}' 2>/dev/null)
if [ -n "$ipb" ] && kubectl exec nettest-a -- ping -c2 -W2 "$ipb" >/dev/null 2>&1; then
  ok "pod on $n0 reached pod on $n1 ($ipb) across nodes"
else bad "cross-node ping failed (flannel 8472/udp blocked? CLUSTER_IFACE wrong?)"; fi

log "3) Knative hello-world (scale-from-zero + Kourier routing)"
if kubectl get crd services.serving.knative.dev >/dev/null 2>&1; then
  cat <<'YAML' | kubectl apply -f - >/dev/null 2>&1
apiVersion: serving.knative.dev/v1
kind: Service
metadata: { name: helloworld }
spec:
  template:
    spec:
      containers:
        - image: gcr.io/knative-samples/helloworld-go
          env: [ { name: TARGET, value: "inter-node-cluster" } ]
YAML
  kubectl wait ksvc/helloworld --for=condition=Ready --timeout=180s >/dev/null 2>&1 || bad "ksvc not Ready (image pull? gcr.io reachable?)"
  url=$(kubectl get ksvc helloworld -o jsonpath='{.status.url}' 2>/dev/null)
  kip=$(kubectl -n kourier-system get svc kourier -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null)
  host=${url#http://}
  if [ -n "$url" ] && [ -n "$kip" ] && \
     curl -s --max-time 15 -H "Host: $host" "http://$kip" 2>/dev/null | grep -q "Hello"; then
    ok "Knative Service answered at $url (via Kourier $kip)"
  else bad "Knative Service didn't answer (url=$url kourier=$kip) — check: kubectl get ksvc,pods -A"; fi
else bad "Knative CRDs missing — run 03-install-knative.sh"; fi

echo ""
[ "$FAIL" = 0 ] && echo -e "\033[1;32mCLUSTER READY ✓\033[0m  (k8s + Knative up; deploy Cloudburst→k8s/cloudburst, RMMap→k8s/knative)" \
                || { echo -e "\033[1;31missues above ✗\033[0m"; exit 1; }
