#!/usr/bin/env bash
# setup-node.sh — install everything the Faasm track needs on THIS node. Idempotent;
# run once per node (the repo is git-synced; SSH between nodes is publickey-blocked,
# so run locally on each — like rtsfaas/setup-node.sh). Installs the Faaslet runtime
# (wasmtime), the wasm build target (for compiling Faaslet modules), python3, and a
# Redis client lib; then ensures the stage dir exists.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
log() { printf '\033[1;36m[faasm-setup %s]\033[0m %s\n' "$(hostname)" "$*"; }
die() { printf '\033[1;31m[faasm-setup] %s\033[0m\n' "$*" >&2; exit 1; }

# 1. wasmtime (the Faaslet runtime that runs the AOT .cwasm modules)
if command -v "${WASMTIME:-wasmtime}" >/dev/null 2>&1; then
  log "wasmtime present: $(wasmtime --version 2>/dev/null | head -1)"
else
  log "installing wasmtime …"
  curl -sSf https://wasmtime.dev/install.sh | bash || die "wasmtime install failed (offline? install it manually)"
  export PATH="$HOME/.wasmtime/bin:$PATH"
  command -v wasmtime >/dev/null 2>&1 || log "WARN: wasmtime not on PATH — add \$HOME/.wasmtime/bin"
fi

# 2. wasm build target (only needed if you (re)compile Faaslet modules on this node)
if command -v rustc >/dev/null 2>&1; then
  rustup target list --installed 2>/dev/null | grep -qx wasm32-wasip1 \
    || { log "rustup target add wasm32-wasip1"; rustup target add wasm32-wasip1 || true; }
else
  log "NOTE: rustc absent — fine if Faaslet .cwasm modules are staged prebuilt (they are)"
fi

# 3. python3 (agent.py is stdlib-only) + redis client (for any python Faaslet wrapper)
command -v python3 >/dev/null 2>&1 || die "python3 required"
python3 -c "import redis" 2>/dev/null || { log "pip install redis"; python3 -m pip install --quiet redis || true; }

# 4. stage dir + Redis reachability hint
mkdir -p "${STAGE_DIR:-/tmp/faasm_stage}"
if command -v redis-cli >/dev/null 2>&1; then
  redis-cli -h "$REDIS_HOST" -p "$REDIS_PORT" ping >/dev/null 2>&1 \
    && log "Redis reachable at $REDIS_HOST:$REDIS_PORT" \
    || log "WARN: Redis NOT reachable at $REDIS_HOST:$REDIS_PORT (stand up the dedicated box / set REDIS_HOST)"
fi
log "setup done. Start the agent with: $HERE/deploy.sh start"
