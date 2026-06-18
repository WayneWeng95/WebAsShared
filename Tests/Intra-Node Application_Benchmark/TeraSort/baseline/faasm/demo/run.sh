#!/usr/bin/env bash
# run.sh — Faasm-like TeraSort demo sweep across input sizes × shuffle fan-out N.
# Builds the WASM module (wasm32-wasip1) + AOT-compiles it, then for each (size,N)
# runs the driver REPS times and keeps the MEDIAN-e2e row. Each partition/merge
# stage is a fresh wasmtime instance (Faaslet); shuffle state moves through Redis.
#
# Columns: size_mb,workers,topo,e2e_ms,throughput_mb_s,records_per_s,peak_mem_mb,
#          state_kv_mb,checksum
#
# Requires: rustc (+wasm32-wasip1), wasmtime, Redis on 127.0.0.1:6379, python3 redis.
# Usage: ./run.sh "50 250 500" "1 2 4 8 16" 3
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../../.." && pwd)"
export PATH="/opt/myapp/.cargo/bin:$PATH"
export RUSTUP_HOME="/opt/myapp/.rustup" CARGO_HOME="/opt/myapp/.cargo"

SIZES="${1:-50 250 500}"
N_LIST="${2:-1 2 4 8 16}"
REPS="${3:-3}"
CSV="$HERE/results.csv"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }

echo "[faasm-demo] building ts.wasm + ts.cwasm..."
rustc --target wasm32-wasip1 -O "$HERE/ts.rs" -o "$HERE/ts.wasm" 2>&1 | tail -2
"$(command -v wasmtime || echo ~/.wasmtime/bin/wasmtime)" compile "$HERE/ts.wasm" -o "$HERE/ts.cwasm" 2>&1 | tail -1

# Keep the row whose e2e_ms (field 4) is the median across REPS runs.
median_row() { sort -t, -k4,4 -n | awk -v n="$1" 'NR==int((n+1)/2){print}'; }

echo "[faasm-demo] sizes=[$SIZES] N=[$N_LIST] reps=$REPS"
first=1; : > "$CSV"
for s in $SIZES; do
  REC="$WAS_ROOT/TestData/terasort_${s}mb.dat"
  [ -f "$REC" ] || { echo "  SKIP (missing): $REC"; continue; }
  for N in $N_LIST; do
    [ "$first" = 1 ] && { echo "size_mb,workers,topo,e2e_ms,throughput_mb_s,records_per_s,peak_mem_mb,state_kv_mb,checksum" | tee -a "$CSV"; first=0; }
    rows="$(mktemp)"
    for _ in $(seq 1 "$REPS"); do
      TS_RECORDS="$REC" TS_NUM_WORKERS="$N" REDIS_DB=3 python3 "$HERE/driver.py" "$N" >> "$rows" 2>/dev/null
    done
    median_row "$REPS" < "$rows" | tee -a "$CSV"
    rm -f "$rows"
  done
done
echo "[faasm-demo] wrote $CSV"
