#!/usr/bin/env bash
# run.sh — Faasm inter-node WordCount SWEEP. Establishes ground truth (1 mapper on
# node 0), then sweeps the mapper count across the cluster, gating total_occurrences
# (fan-out-invariant) at every point and recording the makespan scaling curve into
# results.csv.
#
# Run on NODE 0 — the driver reads the corpus here and pushes chunks via Redis;
# workers pull their chunks from Redis (the corpus itself is not needed on workers,
# and large corpora aren't git-synced).
#
# Usage:  ./run.sh [CORPUS] [MAPPERS_LIST] [REPS]
#   ./run.sh                                          # corpus_large, "1 2 4 8 16", 3 reps
#   ./run.sh "$PWD/../../../TestData/corpus_xlarge.txt" "4 8 16 32" 5
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
CORPUS="${1:-$ROOT/TestData/corpus_large.txt}"
MAPPERS_LIST="${2:-1 2 4 8 16}"
REPS="${3:-3}"
CSV="$HERE/results.csv"
log() { printf '\033[1;35m[wc-faasm]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[wc-faasm] %s\033[0m\n' "$*" >&2; exit 1; }

[ -f "$CORPUS" ] || die "corpus not found: $CORPUS
  → run this on NODE 0 (the corpus lives there; large files aren't git-synced).
    Or distribute it first: ../../Inter-Node\\ deployment/faasm/deploy.sh distribute $CORPUS"

log "ground truth — 1 mapper on node 0"
GT_OUT="$(python3 "$HERE/run_wordcount.py" --corpus "$CORPUS" --mappers 1 --nodes 1 --reps 1 \
            --csv "$HERE/.gt.csv")" || { echo "$GT_OUT"; exit 1; }
echo "$GT_OUT"
OCC_GT="$(printf '%s\n' "$GT_OUT" | sed -n 's/^RESULT occ=\([0-9]*\).*/\1/p')"
[ -n "$OCC_GT" ] || die "could not read ground-truth occurrences"
log "ground-truth occurrences = $OCC_GT"

# Fresh sweep CSV (header written by the driver on first append).
rm -f "$CSV"
log "sweep mappers=[$MAPPERS_LIST] across 4 nodes, reps=$REPS, gate vs $OCC_GT"
fail=0
for M in $MAPPERS_LIST; do
  python3 "$HERE/run_wordcount.py" --corpus "$CORPUS" --mappers "$M" --nodes 4 \
          --reps "$REPS" --expect "$OCC_GT" --csv "$CSV" || fail=1
done

echo ""
log "=== scaling curve (mappers vs makespan) ==="
column -t -s, "$CSV" 2>/dev/null || cat "$CSV"
echo ""
[ "$fail" -eq 0 ] && log "SWEEP PASS ✓  (occurrences match GT at every point; see results.csv)" \
                  || { log "some cells FAILED ✗"; exit 1; }
