#!/usr/bin/env bash
# run.sh — RMMap-ES (parallel pods) MNIST-inference sweep. One fresh process per (size,W) cell; rows
# into results.csv. Same integer forward pass as the guest → identical checksum gate.
# Columns: size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,kvs_ser_mb,checksum,accuracy
# Requires: redis-server on 127.0.0.1:6379, numpy.
# Usage: ./run.sh ["100000 300000 600000"] ["1 2 4 8 16"]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../.." && pwd)"
GENDATA="$WAS_ROOT/Tests/Inter-Node Application_Benchmark/ML_training/gen_data.py"
GENMODEL="$WAS_ROOT/Tests/Inter-Node Application_Benchmark/ML_inference/gen_model.py"
DATADIR="$WAS_ROOT/TestData/ml"; SEED="${MLI_SEED:-9999}"
SIZES="${1:-100000 300000 600000}"; W_LIST="${2:-1 2 4 8 16}"; CSV="$HERE/results.csv"
redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }
mkdir -p "$DATADIR"
MODEL="$DATADIR/infer_model.txt"; [ -f "$MODEL" ] || python3 "$GENMODEL" "$MODEL" >/dev/null

first=1; : > "$CSV"
echo "[rmmap] sizes=[$SIZES] W=[$W_LIST]"
for NS in $SIZES; do
  DATA="$DATADIR/test_${NS}.csv"; [ -f "$DATA" ] || python3 "$GENDATA" "$NS" "$DATA" "$SEED" >/dev/null
  bytes=$(wc -c < "$DATA"); size_mb=$(awk "BEGIN{printf \"%.1f\", $bytes/1048576}")
  for W in $W_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    MLI_MODEL="$MODEL" MLI_DATA="$DATA" python3 "$HERE/driver.py" "$size_mb" "$NS" "$W" $hdr | tee -a "$CSV"
    first=0; redis-cli flushdb >/dev/null 2>&1 || true
  done
done
echo "[rmmap] wrote $CSV"
