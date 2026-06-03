#!/usr/bin/env bash
# run.sh — sweep the INPUT FILE SIZE for the 2-node RDMA distributed word count
# (DAGs/symbolic_dag/word_count.json) and record per-node + total wall times.
#
# For each size it regenerates TestData/corpus_large.txt (line-aligned), submits
# the SymbolicDag to the running coordinator, and parses the job-result summary:
#
#     node 0 (local): <N>ms        ← producer: local count + RDMA send
#     node 1 (worker): <N>ms       ← receiver + global reducer (critical path)
#     total wall time: <N>ms
#
# Prerequisites (must already be running — this only submits jobs):
#   node 0 (10.10.1.2):  ./node-agent start --config NodeAgent/agent_coordinator.toml
#   node 1 (10.10.1.1):  ./node-agent start --config NodeAgent/agent_worker.toml
#
# corpus_large.txt is only needed on node 0 (source_node:0); staging delivers it
# to node 1 over RDMA (TCP fallback if RDMA is unavailable).
#
# Note: both nodes load+count the FULL file (the demo double-counts and is NOT
# input-partitioned), so each node processes the whole corpus — keep sizes within
# the ~1.5 GiB guest window (≤ ~512 MB).
#
# Usage:
#   ./run.sh                     # default sweep: 16 32 64 128 256 MB
#   ./run.sh "64 256 512"        # custom sizes (MB)
#   ./run.sh "128" 5             # size list + repeats per size (median reported)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
DAG="DAGs/symbolic_dag/word_count.json"
CFG="NodeAgent/agent_coordinator.toml"
SEED="$ROOT/TestData/corpus.txt"
TARGET="$ROOT/TestData/corpus_large.txt"
CSV="$HERE/results.csv"

SIZES_MB="${1:-16 32 64 128 256}"
REPEATS="${2:-1}"

log() { printf '\033[1;36m[rdma-scale]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[rdma-scale] %s\033[0m\n' "$*" >&2; exit 1; }

[ -f "$SEED" ] || die "seed corpus not found: $SEED"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:int((a[NR/2]+a[NR/2+1])/2)}'; }

regen() { # bytes -> line-aligned TestData/corpus_large.txt
  head -c "$1" "$SEED" > "$TARGET"
  python3 - "$TARGET" <<'PY'
import sys
p=sys.argv[1]; d=open(p,'rb').read(); nl=d.rfind(b'\n')
open(p,'wb').write(d[:nl+1] if nl>=0 else d)
PY
}

# Extract the latency (the number before "ms"), NOT the node index in the label.
parse() { echo "$1" | grep -oE "$2: [0-9]+ms" | grep -oE "[0-9]+ms" | grep -oE "[0-9]+" | head -1; }

echo "size_mb,size_bytes,node0_ms,node1_ms,total_ms,ratio_n1_n0" > "$CSV"
log "sweep sizes (MB): $SIZES_MB   repeats=$REPEATS"
printf "%6s  %10s  %9s  %9s  %9s  %7s\n" "MB" "bytes" "node0ms" "node1ms" "totalms" "n1/n0"

for mb in $SIZES_MB; do
  regen $((mb * 1024 * 1024))
  sz=$(wc -c < "$TARGET")
  n0s=(); n1s=(); tots=()
  for _ in $(seq 1 "$REPEATS"); do
    out=$(cd "$ROOT" && ./node-agent submit --config "$CFG" --dag "$DAG" 2>&1) || { echo "$out" | tail -3; die "submit failed at ${mb}MB"; }
    n0=$(parse "$out" "node 0 \(local\)"); n1=$(parse "$out" "node 1 \(worker\)"); t=$(parse "$out" "total wall time")
    [ -n "$n0" ] && n0s+=("$n0"); [ -n "$n1" ] && n1s+=("$n1"); [ -n "$t" ] && tots+=("$t")
  done
  n0=$(median "${n0s[@]}"); n1=$(median "${n1s[@]}"); tot=$(median "${tots[@]}")
  ratio=$(awk "BEGIN{printf \"%.2f\", $n1/$n0}")
  echo "$mb,$sz,$n0,$n1,$tot,$ratio" >> "$CSV"
  printf "%6s  %10s  %9s  %9s  %9s  %7s\n" "$mb" "$sz" "$n0" "$n1" "$tot" "$ratio"
done

log "wrote $CSV"
