#!/usr/bin/env bash
# run.sh — Cloudburst-on-Redis WordCount sweep. Loops the mapper count N, one
# FRESH python process per N (so peak_mem_mb is a clean per-N high-water), and
# concatenates the rows into results.csv.
#
# Requires: redis-server up on 127.0.0.1:6379, cloudpickle installed.
#
# Usage:
#   ./run.sh                                  # 50MB corpus, N=1..16, 3 requests
#   ./run.sh CORPUS_PATH "1 2 4 8 16" 3
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../.." && pwd)"

CORPUS="${1:-$WAS_ROOT/TestData/corpus_large.txt}"
N_LIST="${2:-1 2 4 8 16}"
REQS="${3:-3}"
CSV="$HERE/results.csv"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }

first=1
: > "$CSV"
echo "corpus=$CORPUS  N=[$N_LIST]  requests=$REQS"
for N in $N_LIST; do
  if [ "$first" = 1 ]; then
    python3 "$HERE/run_redis.py" "$CORPUS" "$N" "$REQS" --header | tee -a "$CSV"
    first=0
  else
    python3 "$HERE/run_redis.py" "$CORPUS" "$N" "$REQS" | tee -a "$CSV"
  fi
done
echo "wrote $CSV"
