#!/usr/bin/env bash
# gen_dags.sh — generate + offline-partition the balanced ClusterDag for every
# Inter-Node Application Benchmark workload, ready for `node-agent submit`.
#
# This benchmark targets MAKESPAN, so every workload uses the **balanced** placement
# policy (the makespan-favouring distributing policy) — there is NO policy sweep
# here (that's the Scheduling-Policy experiment, which is done). For each workload
# we run the same offline pipeline the cluster path expects:
#
#     gen_variants.py <wl> [balanced] --nodes N ... --out <sym>   # balanced is the default
#     partition <sym> --nodes N > <cdag>                          # embedded placement authoritative
#     node-agent submit --config agent.toml --dag <cdag>          # (run separately, on the cluster)
#
# Pre-partitioning OFFLINE keeps the embedded balanced placement authoritative — on
# a live cluster the coordinator's SCX hints would otherwise override it.
#
# Usage:
#   ./gen_dags.sh [NODES]              # default NODES=4
#   NODES=9 ./gen_dags.sh              # 9-node full run
#   FANOUT=16 ./gen_dags.sh 4          # override the fan width for the fan workloads
#
# Outputs sym + cdag pairs into ./dags/ . Inputs must be staged on every node first
# (see README.md for the per-workload paths).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
# A function (not a string) so the space in the "Inter-Node Application_Benchmark"
# path can't word-split when invoked.
gv() { python3 "$HERE/gen_variants.py" "$@"; }
PART="$ROOT/Partitioner/target/release/partition"
OUT="$HERE/dags"

NODES="${NODES:-${1:-4}}"
FANOUT="${FANOUT:-8}"          # fan width for word_count/finra/ml_training/ml_inference
PACK_CAP="${PACK_CAP:-16}"     # per-node worker cap (cores/node)
MATRIX_N="${MATRIX_N:-512}"    # matrix dimension (must match the staged A/B inputs)
MATRIX_W="${MATRIX_W:-4}"      # matrix block count (r·c grid)
TS_FANOUT="${TS_FANOUT:-$NODES}"  # terasort: N==nodes is the deadlock-safe square layout

mkdir -p "$OUT"
[ -x "$PART" ] || { echo "partition binary not built ($PART) — run ./build.sh"; exit 1; }
pass=0; fail=0

# gen+partition one workload at balanced. $1=name, rest=extra gen_variants args.
emit() {
  local name="$1"; shift
  local sym="$OUT/$name.sym.json" cdag="$OUT/$name.cdag.json"
  if ! gv "$name" --nodes "$NODES" "$@" --out "$sym" >/tmp/gd_gen.err 2>&1; then
    echo "  ✗ $name  (gen failed)"; sed 's/^/      /' /tmp/gd_gen.err; fail=$((fail+1)); return
  fi
  if ! "$PART" "$sym" --nodes "$NODES" > "$cdag" 2>/tmp/gd_part.err; then
    echo "  ✗ $name  (partition failed)"; sed 's/^/      /' /tmp/gd_part.err; fail=$((fail+1)); return
  fi
  echo "  ✓ $name  → dags/$name.cdag.json"; pass=$((pass+1))
}

echo "Generating balanced ClusterDags  (NODES=$NODES, fan FANOUT=$FANOUT)"
emit word_count   --input TestData/corpus_xlarge.txt --fanout "$FANOUT"
emit finra        --fanout "$FANOUT" --pack-cap "$PACK_CAP"
emit ml_training  --fanout "$FANOUT"
emit ml_inference --fanout "$FANOUT"
emit matrix       --fanout "$MATRIX_W" --matrix-n "$MATRIX_N"
emit terasort     --fanout "$TS_FANOUT"

echo ""
echo "PASS=$pass FAIL=$fail   (cdags in $OUT/)"
[ "$fail" -eq 0 ] || exit 1
