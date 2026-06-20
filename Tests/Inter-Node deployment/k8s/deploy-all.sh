#!/usr/bin/env bash
# deploy-all.sh — ONE command to stand up the whole k8s environment for every
# k8s-based test framework:
#     k3s  →  Knative+Kourier (RMMap)  →  Strimzi+Kafka (Flink StateFun)
#           →  Redis (Cloudburst + RMMap shared KV)  →  StateFun image distribution
#           →  verify
#
# Role-aware: run it on ANY node. On the control-plane (SERVER_IP) it builds the
# full server stack; on a worker (an AGENT_IP) it just joins + imports images.
#
#   # node 0 (control-plane) — does everything; brings up workers too IF passwordless
#   # SSH to them works, otherwise prints the one worker command to run on each:
#   ./deploy-all.sh
#
#   # each worker (only needed if SSH was unavailable):
#   ./deploy-all.sh            # auto-reads .cluster-join written by node 0 (copy it over)
#
# Idempotent: every sub-step skips work already done.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
JOIN_FILE="$HERE/.cluster-join"          # gitignored: K3S_URL + K3S_TOKEN for workers
DEPLOY_REDIS="${DEPLOY_REDIS:-1}"        # 1 = in-cluster Redis (testing); 0 = external box
log()  { printf '\033[1;35m[deploy-all %s]\033[0m %s\n' "$(hostname)" "$*"; }
step() { printf '\n\033[1;34m━━ %s ━━\033[0m\n' "$*"; }
die()  { printf '\033[1;31m[deploy-all] %s\033[0m\n' "$*" >&2; exit 1; }
ssh_ok() { timeout 6 ssh -o BatchMode=yes -o ConnectTimeout=5 "$1" true 2>/dev/null; }

# ── role detection ────────────────────────────────────────────────────────────
MYIPS="$(ip -o addr show 2>/dev/null | awk '{print $4}' | cut -d/ -f1)"
ROLE=worker
echo "$MYIPS" | grep -qx "$SERVER_IP" && ROLE=server

# ════════════════════════════════════════════════════════════════ WORKER ══════
if [ "$ROLE" = worker ]; then
  log "role: WORKER"
  if [ -z "${K3S_URL:-}" ] || [ -z "${K3S_TOKEN:-}" ]; then
    [ -f "$JOIN_FILE" ] && source "$JOIN_FILE"
  fi
  [ -n "${K3S_URL:-}" ] && [ -n "${K3S_TOKEN:-}" ] \
    || die "need K3S_URL + K3S_TOKEN (run deploy-all.sh on node 0 first, then copy $JOIN_FILE here — or pass them as env)"
  step "join cluster"; K3S_URL="$K3S_URL" K3S_TOKEN="$K3S_TOKEN" "$HERE/02-install-agent.sh"
  step "import StateFun images (if staged)"; "$HERE/07-load-images.sh" import || log "skip images (no tarballs yet)"
  log "worker done. Verify from node 0: kubectl get nodes -o wide"
  exit 0
fi

# ════════════════════════════════════════════════════════════════ SERVER ══════
log "role: CONTROL-PLANE"
step "1. k3s control-plane";        "$HERE/01-install-server.sh" || die "server install failed"
export KUBECONFIG="$KUBECONFIG_PATH"

# Publish the join creds for workers (gitignored).
{ echo "K3S_URL=https://$SERVER_IP:6443";
  echo "K3S_TOKEN='$(sudo cat /var/lib/rancher/k3s/server/node-token)'"; } > "$JOIN_FILE"
chmod 600 "$JOIN_FILE"

step "2. join workers"
MANUAL=()
for ip in $AGENT_IPS; do
  if ssh_ok "$ip"; then
    log "SSH ok → joining $ip remotely"
    ssh -o BatchMode=yes "$ip" "cd '$HERE' && K3S_URL=https://$SERVER_IP:6443 K3S_TOKEN='$(sudo cat /var/lib/rancher/k3s/server/node-token)' ./02-install-agent.sh" \
      || log "WARN: remote join of $ip failed — add it manually"
  else
    MANUAL+=("$ip")
  fi
done
if [ ${#MANUAL[@]} -gt 0 ]; then
  log "no SSH to: ${MANUAL[*]} — on EACH, copy $JOIN_FILE then run:  ./deploy-all.sh"
  log "waiting up to 5 min for workers to appear (join them now in another shell) …"
  for i in $(seq 1 60); do
    n=$(kubectl get nodes --no-headers 2>/dev/null | grep -cw Ready)
    [ "$n" -ge "$(( 1 + $(echo $AGENT_IPS | wc -w) ))" ] && break
    sleep 5
  done
fi
kubectl get nodes -o wide || true

step "3. Knative Serving + Kourier (RMMap runtime)"; "$HERE/03-install-knative.sh" || die "knative failed"
step "4. Strimzi + Kafka (Flink StateFun runtime)";  "$HERE/06-strimzi.sh"        || die "strimzi failed"
if [ "$DEPLOY_REDIS" = 1 ]; then
  step "5. in-cluster Redis (Cloudburst/RMMap KV — TESTING; use the dedicated box for real runs)"
  "$HERE/05-redis.sh" || log "WARN: redis deploy failed"
else
  log "5. skipping in-cluster Redis (DEPLOY_REDIS=0) — using external REDIS_HOST=$REDIS_HOST"
fi

step "6. StateFun images → every node's containerd"
"$HERE/07-load-images.sh" export || log "skip export (images not built yet — build them in the streaming benchmark)"
"$HERE/07-load-images.sh" import || log "skip local import (no tarballs)"
for ip in $AGENT_IPS; do
  ssh_ok "$ip" && { log "import images on $ip"; ssh -o BatchMode=yes "$ip" "cd '$HERE' && ./07-load-images.sh import" || log "WARN: image import on $ip failed"; }
done

step "7. verify";  "$HERE/04-verify.sh" || die "verification failed — see output above"

step "DONE"
log "k8s env up for: Cloudburst (k8s) · RMMap (Knative) · Flink StateFun (Strimzi/Kafka) · shared Redis"
log "Deploy the framework apps next: cloudburst/  knative/  statefun/ (+ the streaming benchmark's StateFun manifests)"
