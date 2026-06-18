#!/usr/bin/env bash
# run.sh — Cloudburst-on-Redis synchronous-SGD sweep. One FRESH python process
# per (size,W) cell (clean per-cell peak RSS), rows concatenated into results.csv.
#
# Single-process Redis-KVS dataflow (split→gradient→aggregate), every chunk/
# model/gradient round-tripped through Redis (cloudpickle). Honest scope: Redis
# (not Anna), gradient functions sequential (no per-function parallelism — curve
# ~flat in W). Same integer kernel as the guest → identical checksum gate.
#
# Columns: size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,kvs_ser_mb,checksum,accuracy
# Requires: redis-server on 127.0.0.1:6379, numpy (+ cloudpickle if present).
# Usage: ./run.sh ["100000 300000 600000"] ["1 2 4 8 16"]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../.." && pwd)"
GENDATA="$WAS_ROOT/Tests/Application_Benchmark/ML_training/gen_data.py"
DATADIR="$WAS_ROOT/TestData/ml"
SEED="${ML_SEED:-1234}"

SIZES="${1:-100000 300000 600000}"
W_LIST="${2:-1 2 4 8 16}"
CSV="$HERE/results.csv"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }
mkdir -p "$DATADIR"

first=1; : > "$CSV"
echo "[cloudburst] sizes=[$SIZES] W=[$W_LIST] epochs=${ML_EPOCHS:-10}"
for NS in $SIZES; do
  DATA="$DATADIR/sgd_${NS}.csv"
  [ -f "$DATA" ] || python3 "$GENDATA" "$NS" "$DATA" "$SEED" >/dev/null
  bytes=$(wc -c < "$DATA"); size_mb=$(awk "BEGIN{printf \"%.1f\", $bytes/1048576}")
  for W in $W_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    ML_DATA="$DATA" python3 "$HERE/sgd.py" "$size_mb" "$NS" "$W" $hdr | tee -a "$CSV"
    first=0
  done
done
echo "[cloudburst] wrote $CSV"
