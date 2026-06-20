#!/usr/bin/env bash
# run.sh — Faasm inter-node WordCount: ground truth (1 mapper, node 0) vs distributed
# (N mappers across the cluster). Asserts the distributed total_occurrences matches
# ground truth (fan-out-invariant gate), and records the distributed makespan.
#
# Prereqs: agents up (../../Inter-Node deployment/faasm/deploy.sh start) and Redis
# reachable from every node at REDIS_HOST (cluster.env).
#
# Usage:  ./run.sh [CORPUS] [MAPPERS] [REPS]
#   ./run.sh                                   # corpus_large (50MB), 8 mappers, 3 reps
#   ./run.sh ../../../TestData/corpus_xlarge.txt 16 5
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
CORPUS="${1:-$ROOT/TestData/corpus_large.txt}"
MAPPERS="${2:-8}"
REPS="${3:-3}"
log() { printf '\033[1;35m[wc-faasm]\033[0m %s\n' "$*"; }

[ -f "$CORPUS" ] || { echo "corpus not found: $CORPUS" >&2; exit 1; }

log "ground truth — 1 mapper on node 0"
GT_OUT="$(python3 "$HERE/run_wordcount.py" --corpus "$CORPUS" --mappers 1 --nodes 1 --reps 1 \
            --csv "$HERE/.gt.csv")" || { echo "$GT_OUT"; exit 1; }
echo "$GT_OUT"
OCC_GT="$(printf '%s\n' "$GT_OUT" | sed -n 's/^RESULT occ=\([0-9]*\).*/\1/p')"
[ -n "$OCC_GT" ] || { echo "could not read ground-truth occurrences" >&2; exit 1; }
log "ground-truth occurrences = $OCC_GT"

log "distributed — $MAPPERS mappers across the cluster, gate vs $OCC_GT"
python3 "$HERE/run_wordcount.py" --corpus "$CORPUS" --mappers "$MAPPERS" --nodes 4 \
        --reps "$REPS" --expect "$OCC_GT" --csv "$HERE/results.csv"
rc=$?
echo ""
[ $rc -eq 0 ] && log "PASS ✓  (occurrences match GT; see results.csv)" || log "FAIL ✗ (see above)"
exit $rc
