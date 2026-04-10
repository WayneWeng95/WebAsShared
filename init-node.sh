#!/bin/bash
# ============================================================
# init-node.sh
# One-shot setup for a new node: pull latest code, install
# system packages, set up Rust env, install Claude Code,
# and build all projects.
# Usage:  chmod +x init-node.sh && ./init-node.sh
# ============================================================

set -euo pipefail

ROOT=$(cd "$(dirname "$0")" && pwd)
cd "$ROOT"

GREEN='\033[0;32m'
NC='\033[0m'
info() { echo -e "${GREEN}[INFO]${NC}  $*"; }

echo ""
echo "╔══════════════════════════════════════════╗"
echo "║       Node Initialization Script         ║"
echo "╚══════════════════════════════════════════╝"
echo ""

# ── Step 1: Git pull ────────────────────────────────────────
info "Pulling latest code..."
git pull

# ── Step 2: Install RDMA / InfiniBand packages ─────────────
info "Installing system packages (RDMA, InfiniBand, perftest)..."
sudo apt-get update -qq || true
sudo apt-get install -y libibverbs-dev pkg-config librdmacm-dev ibverbs-utils perftest

# ── Step 2b: Configure memlock for RDMA ────────────────────
# ibv_reg_mr pins user pages for DMA — requires RLIMIT_MEMLOCK
# to be at least as large as the SHM region (36 MiB+).
# Without this, ibv_reg_mr fails with ENOMEM.
info "Configuring memlock limits for RDMA..."
LIMITS_FILE="/etc/security/limits.conf"
if ! grep -q '^\*.*memlock.*unlimited' "$LIMITS_FILE" 2>/dev/null; then
    echo '* soft memlock unlimited' | sudo tee -a "$LIMITS_FILE" > /dev/null
    echo '* hard memlock unlimited' | sudo tee -a "$LIMITS_FILE" > /dev/null
    info "Added memlock unlimited to $LIMITS_FILE (takes effect on next login)"
fi
info "Current memlock limit: $(ulimit -l) KB (should be unlimited)"


# ── Step 3: Rust environment setup ──────────────────────────
info "Setting up Rust environment..."
source "$ROOT/scripts/rust.sh"

# ── Step 4: Install wasmtime (for Python/WASM execution) ───
info "Installing wasmtime..."
bash "$ROOT/scripts/install_wasmtime.sh"

# ── Step 5: Claude Code setup ──────────────────────────────
info "Running Claude Code setup..."
bash "$ROOT/scripts/claude-code-setup.sh"

# ── Step 6: Build all projects ──────────────────────────────
info "Building all projects..."
bash "$ROOT/build.sh"

echo ""
info "Node initialization complete!"
