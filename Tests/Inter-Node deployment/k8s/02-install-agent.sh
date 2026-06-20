#!/usr/bin/env bash
# 02-install-agent.sh — join THIS worker node to the k3s cluster.
#
# Run on EACH worker (AGENT_IPS). Needs the server URL + token printed by
# 01-install-server.sh:
#
#   K3S_URL=https://10.10.1.2:6443 K3S_TOKEN='<token>' ./02-install-agent.sh
#
# (If SSH between nodes worked you could push this from node 0; it's publickey-
# blocked on this cluster, so run it locally on each worker — like the rtsfaas
# setup-node.sh flow.)
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
log() { printf '\033[1;36m[k3s-agent %s]\033[0m %s\n' "$(hostname)" "$*"; }
die() { printf '\033[1;31m[k3s-agent] %s\033[0m\n' "$*" >&2; exit 1; }

: "${K3S_URL:?set K3S_URL=https://$SERVER_IP:6443 (from 01-install-server.sh output)}"
: "${K3S_TOKEN:?set K3S_TOKEN=<token> (from 01-install-server.sh output)}"

# This worker's own cluster IP (must be one of AGENT_IPS).
MY_IP="$(ip -o addr show 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | grep -Fx -f <(printf '%s\n' $AGENT_IPS) | head -1)"
[ -n "$MY_IP" ] || die "this host owns none of AGENT_IPS=[$AGENT_IPS] — fix cluster.env"
EXTRA="--node-ip $MY_IP"
[ -n "${CLUSTER_IFACE}" ] && EXTRA="$EXTRA --flannel-iface $CLUSTER_IFACE"

if command -v k3s >/dev/null 2>&1 && sudo systemctl is-active --quiet k3s-agent 2>/dev/null; then
  log "k3s agent already running — skipping (uninstall with /usr/local/bin/k3s-agent-uninstall.sh)"
  exit 0
fi

log "joining $K3S_URL as $MY_IP …"
curl -sfL https://get.k3s.io | \
  INSTALL_K3S_VERSION="${K3S_VERSION}" \
  K3S_URL="$K3S_URL" K3S_TOKEN="$K3S_TOKEN" \
  INSTALL_K3S_EXEC="agent $EXTRA" \
  sh - || die "k3s agent join failed (check token/URL; firewall must allow 6443 + flannel 8472/udp)"

log "joined. Verify from node 0:  kubectl get nodes -o wide   (this node should appear Ready)"
