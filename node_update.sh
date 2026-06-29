#!/usr/bin/env bash
# node_update.sh — bring a node up to date and (re)start its agent.
#
# Run this on EVERY node (coordinator + workers). It:
#   1. pulls the latest code,
#   2. rebuilds all projects (./build.sh),
#   3. starts the node agent (auto-detects coordinator vs worker from agent.toml).
#
# Because /opt/myapp is node-local (not on the shared NFS), every node must run
# this to pick up code changes — otherwise nodes run mismatched binaries (e.g. a
# stale guest.wasm), which corrupts results or wedges the RDMA mesh.
#
# Usage:  ./node_update.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info() { echo -e "${GREEN}[node-update]${NC} $*"; }
warn() { echo -e "${YELLOW}[node-update]${NC} $*"; }

# ── 1. Pull latest code ──────────────────────────────────────────────────────
# SKIP_PULL=1 rebuilds the current tree without pulling (used by
# cluster_start.sh --no-pull).
if [ "${SKIP_PULL:-0}" = "1" ]; then
    warn "SKIP_PULL=1 — building current tree without git pull."
else
    info "Pulling latest code…"
    git pull
fi

# ── 2. Stop any running agent, then rebuild ──────────────────────────────────
# build.sh can't overwrite the node-agent/host binaries while the agent is
# running ("Text file busy"), so stop it first.
if pgrep -f "node-agent (start|--config)" >/dev/null 2>&1; then
    warn "Stopping running node-agent before rebuild…"
    pkill -f "node-agent (start|--config)" || true
    sleep 2
fi

info "Building all projects (./build.sh)…"
./build.sh

# ── 3. Start the node agent ──────────────────────────────────────────────────
# Auto-detects this node's id/role from NodeAgent/agent.toml (node 0 = coordinator).
#
# Two start modes:
#   default     — foreground (`exec`), for interactive use on a node.
#   DETACH=1    — background via `setsid nohup`, logging to $AGENT_LOG, then
#                 return.  Used by cluster_start.sh so the SSH session can close
#                 after the agent is confirmed up.
AGENT_LOG="${AGENT_LOG:-/tmp/node-agent.log}"
if [ "${DETACH:-0}" = "1" ]; then
    info "Starting node agent (detached → $AGENT_LOG)…"
    setsid nohup ./node-agent start --config NodeAgent/agent.toml \
        < /dev/null > "$AGENT_LOG" 2>&1 &
    sleep 1
    if pgrep -f "node-agent (start|--config)" >/dev/null 2>&1; then
        info "node-agent up (pid $(pgrep -f 'node-agent (start|--config)' | head -1)) — log: $AGENT_LOG"
    else
        warn "node-agent failed to start — last log lines:"
        tail -n 15 "$AGENT_LOG" 2>/dev/null || true
        exit 1
    fi
else
    info "Starting node agent…"
    exec ./node-agent start --config NodeAgent/agent.toml
fi
