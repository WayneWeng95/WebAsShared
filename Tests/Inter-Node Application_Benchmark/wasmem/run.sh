#!/usr/bin/env bash
# run.sh — WasMem (auto-placement) inter-node WordCount size sweep, measured the same
# way as ../faasm/run.sh so the two systems compare directly. Fixed fan width
# (default 40 = 10 map workers/node × 4 nodes, balanced policy); sweeps the corpus.
#
# Run on NODE 0. The cluster must be UP first:
#     ./node-agent start --config NodeAgent/agent.toml   # on EVERY node
#     ./node-agent status --config NodeAgent/agent.toml   # node 0 — all workers connected
# Only node 0 needs the corpus files (they are the shared_inputs source, RDMA-staged).
#
# Usage:  ./run.sh [FANOUT] [REPS] [CORPORA...]
#   ./run.sh                       # fanout 40, 3 reps, corpus_large/xlarge/1gb
#   ./run.sh 40 5 TestData/corpus_xlarge.txt
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
FANOUT="${1:-40}"
REPS="${2:-3}"
shift 2 2>/dev/null || true
CORPORA=("$@")
[ ${#CORPORA[@]} -eq 0 ] && CORPORA=(TestData/corpus_large.txt TestData/corpus_xlarge.txt TestData/corpus_1gb.txt)
CSV="$HERE/results.csv"
log() { printf '\033[1;36m[wc-wasmem]\033[0m %s\n' "$*"; }

# Cluster must be reachable before we start timing runs.
if ! timeout 15 "$ROOT/node-agent" status --config "$ROOT/NodeAgent/agent.toml" >/dev/null 2>&1; then
  log "coordinator not reachable — start the cluster first:"
  log "  ./node-agent start --config NodeAgent/agent.toml   (on every node)"
  exit 1
fi

rm -f "$CSV"
log "sweep fanout=$FANOUT reps=$REPS over: ${CORPORA[*]}"
fail=0
for C in "${CORPORA[@]}"; do
  python3 "$HERE/run_wordcount.py" --corpus "$C" --fanout "$FANOUT" --nodes 4 \
          --reps "$REPS" --csv "$CSV" || fail=1
done

echo ""
log "=== makespan + total-job scaling ==="
column -t -s, "$CSV" 2>/dev/null || cat "$CSV"
echo ""
[ "$fail" -eq 0 ] && log "SWEEP PASS ✓" || { log "some cells FAILED ✗"; exit 1; }
