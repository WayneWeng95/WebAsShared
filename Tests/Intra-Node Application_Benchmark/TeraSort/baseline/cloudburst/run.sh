#!/usr/bin/env bash
# run.sh — Cloudburst-on-Redis TeraSort sweep across input sizes × worker count N.
# One FRESH python process per (size,N) (clean per-cell peak RSS); run_redis.py
# does NUM_REQUESTS internally and reports the median. Rows → results.csv.
#
# Requires: redis-server up on 127.0.0.1:6379, cloudpickle installed.
#
# Usage:
#   ./run.sh                                   # 50 250 500 MB, N=1..16, 3 reqs
#   ./run.sh "50 250 500" "1 2 4 8 16" 3
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../.." && pwd)"

SIZES="${1:-50 250 500}"
N_LIST="${2:-1 2 4 8 16}"
REQS="${3:-3}"
CSV="$HERE/results.csv"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }

first=1
: > "$CSV"
echo "cloudburst: sizes=[$SIZES] N=[$N_LIST] requests=$REQS"
for s in $SIZES; do
  REC="$WAS_ROOT/TestData/terasort_${s}mb.dat"
  [ -f "$REC" ] || { echo "  SKIP (missing): $REC (run TeraSort/gen_records.py $s $REC)"; continue; }
  for N in $N_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    REDIS_DB=0 python3 "$HERE/run_redis.py" "$REC" "$N" "$REQS" $hdr | tee -a "$CSV"
    first=0
  done
done
echo "wrote $CSV"
