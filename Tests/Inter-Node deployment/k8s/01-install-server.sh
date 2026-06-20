#!/usr/bin/env bash
# 01-install-server.sh — bring up the k3s CONTROL-PLANE on node 0 (SERVER_IP).
#
# Run THIS on node 0 only. k3s bundles containerd + flannel CNI + a service LB, so
# pod-to-pod across nodes and LoadBalancer Services work out of the box — no extra
# CNI to install (track B of EXPERIMENT_PLAN.md). Traefik is disabled because
# Knative uses its own ingress (Kourier, installed by 03-install-knative.sh).
#
# Idempotent: re-running re-applies the same install. After it finishes it prints
# the exact join command (URL + token) to paste into 02-install-agent.sh on each
# worker.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
log() { printf '\033[1;36m[k3s-server %s]\033[0m %s\n' "$(hostname)" "$*"; }
die() { printf '\033[1;31m[k3s-server] %s\033[0m\n' "$*" >&2; exit 1; }

# Sanity: this box must own SERVER_IP.
ip -o addr show 2>/dev/null | grep -qw "$SERVER_IP" \
  || die "this host does not own SERVER_IP=$SERVER_IP — run 01 on node 0, or fix cluster.env"

# Pin node-ip (and flannel) to the cluster network so k8s traffic uses the right NIC.
EXTRA="--node-ip $SERVER_IP --advertise-address $SERVER_IP"
[ -n "${CLUSTER_IFACE}" ] && EXTRA="$EXTRA --flannel-iface $CLUSTER_IFACE"

if command -v k3s >/dev/null 2>&1 && sudo systemctl is-active --quiet k3s 2>/dev/null; then
  log "k3s server already running — skipping install (delete with /usr/local/bin/k3s-uninstall.sh to redo)"
else
  log "installing k3s server ${K3S_VERSION:-(latest)} …"
  # --disable traefik       : Knative brings Kourier; avoid ingress conflict.
  # --write-kubeconfig-mode : let non-root read the kubeconfig.
  curl -sfL https://get.k3s.io | \
    INSTALL_K3S_VERSION="${K3S_VERSION}" \
    INSTALL_K3S_EXEC="server --disable traefik --write-kubeconfig-mode 644 $EXTRA" \
    sh - || die "k3s install failed (offline cluster? see README 'Air-gapped')"
fi

export KUBECONFIG="$KUBECONFIG_PATH"
log "waiting for the control-plane node to be Ready …"
for i in $(seq 1 60); do
  kubectl get node >/dev/null 2>&1 && kubectl wait --for=condition=Ready node --all --timeout=5s >/dev/null 2>&1 && break
  sleep 2
done
kubectl get nodes -o wide || die "control-plane not Ready (check: sudo journalctl -u k3s -e)"

TOKEN="$(sudo cat /var/lib/rancher/k3s/server/node-token)"
log "control-plane up. KUBECONFIG=$KUBECONFIG_PATH"
echo ""
echo "════════════════════════════════════════════════════════════════════════"
echo " On EACH worker ($AGENT_IPS) run:"
echo ""
echo "   K3S_URL=https://$SERVER_IP:6443 K3S_TOKEN='$TOKEN' \\"
echo "     Tests/Inter-Node\\ deployment/k8s/02-install-agent.sh"
echo ""
echo " (token also at: sudo cat /var/lib/rancher/k3s/server/node-token)"
echo "════════════════════════════════════════════════════════════════════════"
