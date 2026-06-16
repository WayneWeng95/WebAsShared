#!/usr/bin/env bash
# run.sh — Faasm-like MNIST-inference demo sweep. Builds infer_predict.wasm +
# AOT-compiles it, then sweeps size × W; one fresh process per cell. Each shard's
# forward pass is a fresh wasmtime Faaslet; model/shard/result move through Redis.
# Columns: size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,kvs_ser_mb,checksum,accuracy
# Requires: rustc (+wasm32-wasip1), wasmtime, redis on 127.0.0.1:6379, numpy+redis.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../../.." && pwd)"
export PATH="/opt/myapp/.cargo/bin:$PATH"; export RUSTUP_HOME="/opt/myapp/.rustup" CARGO_HOME="/opt/myapp/.cargo"
GENDATA="$WAS_ROOT/Tests/Application_Benchmark/ML_training/gen_data.py"
GENMODEL="$WAS_ROOT/Tests/Application_Benchmark/ML_inference/gen_model.py"
DATADIR="$WAS_ROOT/TestData/ml"; SEED="${MLI_SEED:-9999}"
SIZES="${1:-100000 300000 600000}"; W_LIST="${2:-1 2 4 8 16}"; CSV="$HERE/results.csv"
redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }
mkdir -p "$DATADIR"; MODEL="$DATADIR/infer_model.txt"; [ -f "$MODEL" ] || python3 "$GENMODEL" "$MODEL" >/dev/null
echo "[faasm-demo] building infer_predict.wasm + .cwasm..."
rustc --target wasm32-wasip1 -O "$HERE/infer_predict.rs" -o "$HERE/infer_predict.wasm" 2>&1 | tail -2
"$HOME/.wasmtime/bin/wasmtime" compile "$HERE/infer_predict.wasm" -o "$HERE/infer_predict.cwasm" 2>&1 | tail -1
first=1; : > "$CSV"; echo "[faasm-demo] sizes=[$SIZES] W=[$W_LIST]"
for NS in $SIZES; do
  DATA="$DATADIR/test_${NS}.csv"; [ -f "$DATA" ] || python3 "$GENDATA" "$NS" "$DATA" "$SEED" >/dev/null
  bytes=$(wc -c < "$DATA"); size_mb=$(awk "BEGIN{printf \"%.1f\", $bytes/1048576}")
  for W in $W_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    MLI_MODEL="$MODEL" MLI_DATA="$DATA" python3 "$HERE/driver.py" "$size_mb" "$NS" "$W" $hdr | tee -a "$CSV"
    first=0; redis-cli flushdb >/dev/null 2>&1 || true
  done
done
echo "[faasm-demo] wrote $CSV"
