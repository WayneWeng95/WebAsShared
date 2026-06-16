#!/usr/bin/env bash
# run.sh — Faasm-like matrix-multiply demo sweep. Builds the WASM module
# (wasm32-wasip1) + AOT-compiles it, then sweeps matrix size N × block-grid
# workers, one fresh process per cell (clean per-N peak RSS), into results.csv.
#
# Each block multiply runs as a fresh wasmtime instance (Faaslet); A/B panels and
# C blocks move through Redis (serialized). Requires: rustc (+wasm32-wasip1),
# wasmtime, a Redis on 127.0.0.1:6379, python3 numpy+redis.
#
# Columns: size_n,workers,topo,e2e_ms,gflops,peak_mem_mb,state_kv_mb,checksum
#
# Usage: ./run.sh ["1024 2048 4096"] ["1 2 4 8 16"]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../../.." && pwd)"
export PATH="/opt/myapp/.cargo/bin:$PATH"
export RUSTUP_HOME="/opt/myapp/.rustup" CARGO_HOME="/opt/myapp/.cargo"

SIZES="${1:-512 1024 2048}"
W_LIST="${2:-1 2 4 8 16}"
CSV="$HERE/results.csv"
DATADIR="$WAS_ROOT/TestData/matrix"
GENMAT="$WAS_ROOT/Tests/Application_Benchmark/Matrix/gen_matrix.py"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }

echo "[faasm-demo] building matblock.wasm + matblock.cwasm..."
rustc --target wasm32-wasip1 -O "$HERE/matblock.rs" -o "$HERE/matblock.wasm" 2>&1 | tail -2
"$HOME/.wasmtime/bin/wasmtime" compile "$HERE/matblock.wasm" -o "$HERE/matblock.cwasm" 2>&1 | tail -1

first=1; : > "$CSV"
echo "[faasm-demo] sizes=[$SIZES] W=[$W_LIST]"
for N in $SIZES; do
  A="$DATADIR/A_${N}.bin"; B="$DATADIR/B_${N}.bin"
  if [ ! -f "$A" ] || [ ! -f "$B" ]; then
    python3 "$GENMAT" "$N" "$A" "$B" >/dev/null || { echo "  gen failed N=$N"; continue; }
  fi
  for W in $W_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    if ! MAT_A="$A" MAT_B="$B" python3 "$HERE/driver.py" "$N" "$W" $hdr 2>/tmp/mat_faasm_err.log | tee -a "$CSV"; then
      reason=$(grep -oiE "memory|alloc|panic|short" /tmp/mat_faasm_err.log | head -1)
      echo "$N,$W,wasm-redis,CRASH,,,,${reason:-fail}" | tee -a "$CSV"
    fi
    first=0
  done
done
echo "[faasm-demo] wrote $CSV"
