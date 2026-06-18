#!/usr/bin/env bash
# run.sh — RMMap (dmerge) TeraSort, ES protocol sweep across input sizes × N.
#
# Docker isn't usable here (no daemon), so this runs the self-contained ES driver
# (driver.py) directly against the local Redis — same ES data path (Redis SET/GET
# + pickle), same profile buckets. The driver times one e2e run; we run REPS per
# (size,N) and keep the MEDIAN-e2e row. One fresh process per run (clean peak RSS).
#
# Columns: size_mb,workers,topo,e2e_ms,throughput_mb_s,records_per_s,peak_mem_mb,
#          exec_ms,sd_ms,es_ms,kvs_ser_mb,checksum
#   sd_ms = pickle serialize/deserialize, es_ms = Redis round-trip — the
#   serialized inter-stage transfer our zero-copy page-chain reports as 0.
#
# Requires: redis-server on 127.0.0.1:6379.
# Usage: ./run.sh "50 250 500" "1 2 4 8 16" 3
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../.." && pwd)"

SIZES="${1:-50 250 500}"
N_LIST="${2:-1 2 4 8 16}"
REPS="${3:-3}"
CSV="$HERE/results.csv"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }

# Keep the row whose e2e_ms (field 4) is the median across REPS runs.
median_row() { sort -t, -k4,4 -n | awk -v n="$1" 'NR==int((n+1)/2){print}'; }

echo "rmmap-es: sizes=[$SIZES] N=[$N_LIST] reps=$REPS"
first=1
: > "$CSV"
for s in $SIZES; do
  REC="$WAS_ROOT/TestData/terasort_${s}mb.dat"
  [ -f "$REC" ] || { echo "  SKIP (missing): $REC"; continue; }
  for N in $N_LIST; do
    [ "$first" = 1 ] && { echo "size_mb,workers,topo,e2e_ms,throughput_mb_s,records_per_s,peak_mem_mb,exec_ms,sd_ms,es_ms,kvs_ser_mb,checksum" | tee -a "$CSV"; first=0; }
    rows="$(mktemp)"
    for _ in $(seq 1 "$REPS"); do
      REDIS_DB=2 TS_RECORDS="$REC" python3 "$HERE/driver.py" "$N" >> "$rows" 2>/dev/null
    done
    median_row "$REPS" < "$rows" | tee -a "$CSV"
    rm -f "$rows"
  done
done
echo "wrote $CSV"
