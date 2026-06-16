#!/usr/bin/env bash
# run.sh — Faasm-like synchronous-SGD demo sweep. Builds the WASM gradient module
# (wasm32-wasip1) + AOT-compiles it, then sweeps dataset size × W, one fresh
# process per cell, into results.csv.
#
# Each gradient computation runs as a fresh wasmtime instance (Faaslet); the data
# shard / model / gradient move through Redis (serialized state). Same integer
# kernel as the WebAsShared guest → identical checksum gate.
#
# Columns: size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,kvs_ser_mb,checksum,accuracy
# Requires: rustc (+wasm32-wasip1), wasmtime, redis on 127.0.0.1:6379, numpy+redis.
# Usage: ./run.sh ["100000 300000 600000"] ["1 2 4 8 16"]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../../.." && pwd)"
export PATH="/opt/myapp/.cargo/bin:$PATH"
export RUSTUP_HOME="/opt/myapp/.rustup" CARGO_HOME="/opt/myapp/.cargo"
GENDATA="$WAS_ROOT/Tests/Application_Benchmark/ML_training/gen_data.py"
DATADIR="$WAS_ROOT/TestData/ml"
SEED="${ML_SEED:-1234}"

SIZES="${1:-100000 300000 600000}"
W_LIST="${2:-1 2 4 8 16}"
CSV="$HERE/results.csv"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }
mkdir -p "$DATADIR"

echo "[faasm-demo] building sgd_grad.wasm + sgd_grad.cwasm..."
rustc --target wasm32-wasip1 -O "$HERE/sgd_grad.rs" -o "$HERE/sgd_grad.wasm" 2>&1 | tail -2
"$HOME/.wasmtime/bin/wasmtime" compile "$HERE/sgd_grad.wasm" -o "$HERE/sgd_grad.cwasm" 2>&1 | tail -1

first=1; : > "$CSV"
echo "[faasm-demo] sizes=[$SIZES] W=[$W_LIST] epochs=${ML_EPOCHS:-10}"
for NS in $SIZES; do
  DATA="$DATADIR/sgd_${NS}.csv"
  [ -f "$DATA" ] || python3 "$GENDATA" "$NS" "$DATA" "$SEED" >/dev/null
  bytes=$(wc -c < "$DATA"); size_mb=$(awk "BEGIN{printf \"%.1f\", $bytes/1048576}")
  for W in $W_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    if ! ML_DATA="$DATA" python3 "$HERE/driver.py" "$size_mb" "$NS" "$W" $hdr 2>/tmp/ml_faasm_err.log | tee -a "$CSV"; then
      reason=$(grep -oiE "memory|alloc|panic|short" /tmp/ml_faasm_err.log | head -1)
      echo "$size_mb,$W,wasm-faaslet-redis,CRASH,,,,${reason:-fail},," | tee -a "$CSV"
    fi
    first=0
    redis-cli flushdb >/dev/null 2>&1 || true
  done
done
echo "[faasm-demo] wrote $CSV"
