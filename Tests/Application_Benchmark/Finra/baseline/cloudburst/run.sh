#!/usr/bin/env bash
# run.sh — Cloudburst-on-Redis FINRA sweep. One fresh process per trades file
# (clean per-size peak RSS) → results.csv.
# Requires redis-server on :6379, cloudpickle installed.
#   ./run.sh "10000 100000 1000000" 3
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS="$(cd "$HERE/../../../../.." && pwd)"
SIZES="${1:-10000 100000 1000000}"; REQS="${2:-3}"; CSV="$HERE/results.csv"
redis-cli ping >/dev/null 2>&1 || { echo "redis not on :6379" >&2; exit 1; }
first=1; : > "$CSV"
for n in $SIZES; do
  f="$WAS/TestData/finra/trades_${n}.csv"
  [ -f "$f" ] || { echo "skip (missing) $f"; continue; }
  hdr=""; [ "$first" = 1 ] && hdr="--header"
  python3 "$HERE/run_redis.py" "$f" "$REQS" $hdr 2>/dev/null | tee -a "$CSV"; first=0
done
echo "wrote $CSV"
