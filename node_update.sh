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
info "Pulling latest code…"
git pull

# ── 2.rebuild ──────────────────────────────────

info "Building all projects (./build.sh)…"
./build.sh

# ── 3. Start the node agent ──────────────────────────────────────────────────
# Auto-detects this node's id/role from NodeAgent/agent.toml (node 0 = coordinator).
info "Starting node agent…"
exec ./node-agent start --config NodeAgent/agent.toml
