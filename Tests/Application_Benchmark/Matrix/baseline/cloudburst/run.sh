#!/usr/bin/env bash
# run.sh — Cloudburst-on-Redis SUMMA matrix-multiply sweep. Loops matrix size N ×
# block-grid W, one FRESH python process per cell (clean per-cell peak RSS), and
# concatenates rows into results.csv.
#
# Requires: redis-server on 127.0.0.1:6379, cloudpickle + numpy installed.
#
# Columns: size_n,workers,topo,e2e_ms_median,gflops,peak_mem_mb,reps,
#          kvs_put_mb,kvs_get_mb,checksum
#
# Usage: ./run.sh ["1024 2048 4096"] ["1 2 4 8 16"] [reqs]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../.." && pwd)"
GENMAT="$WAS_ROOT/Tests/Application_Benchmark/Matrix/gen_matrix.py"

SIZES="${1:-512 1024 2048}"
W_LIST="${2:-1 2 4 8 16}"
REQS="${3:-3}"
CSV="$HERE/results.csv"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }

first=1; : > "$CSV"
echo "[cloudburst] sizes=[$SIZES] W=[$W_LIST] reqs=$REQS"
for N in $SIZES; do
  A="$WAS_ROOT/TestData/matrix/A_${N}.bin"; B="$WAS_ROOT/TestData/matrix/B_${N}.bin"
  if [ ! -f "$A" ] || [ ! -f "$B" ]; then python3 "$GENMAT" "$N" "$A" "$B" >/dev/null; fi
  for W in $W_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    python3 "$HERE/run_redis.py" "$N" "$W" "$REQS" $hdr | tee -a "$CSV"
    first=0
  done
done
echo "[cloudburst] wrote $CSV"
