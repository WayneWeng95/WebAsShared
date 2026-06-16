#!/usr/bin/env bash
# prepartition.sh — generate + offline-partition every (workload × policy) cell.
#
# Produces, for each of the 3 workloads × 4 policies = 12 cells:
#   variants/<wl>__<pol>.symbolic.json   the policy-sensitive SymbolicDag
#   cluster_dags/<wl>__<pol>.json        the partitioned ClusterDag (submit this)
# and writes placement.csv with the static scheduling-decision metrics.
#
# Offline partitioning (the `partition` binary with NO --hints) is what makes the
# embedded placement_policy authoritative: on a live cluster the coordinator hands
# the partitioner SCX-derived capacity hints, and hints.or(policy_hints) means
# those would override the policy (splitter.rs:142). By baking the placement here
# and submitting a finished ClusterDag, resolve_dag() passes it through untouched.
#
# Usage: ./prepartition.sh [NODES]   (default NODES=4)
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
NODES="${1:-4}"

WORKLOADS="word_count finra ml_training"
POLICIES="pack balanced spread random"

PART="$ROOT/Partitioner/target/release/partition"

log() { printf '\033[1;36m[prep]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[prep] %s\033[0m\n' "$*" >&2; exit 1; }

if [ ! -x "$PART" ]; then
  log "partition binary missing — building (cargo build -p partitioner --release)"
  ( cd "$ROOT/Partitioner" && cargo build -p partitioner --release ) || die "build failed"
fi
[ -x "$PART" ] || die "partition binary still missing: $PART"

mkdir -p "$HERE/variants" "$HERE/cluster_dags"
CSV="$HERE/placement.csv"
echo "workload,policy,nodes,busy_hosts,imbalance,cross_node_edges,compute_per_host" > "$CSV"

log "nodes=$NODES  workloads=[$WORKLOADS]  policies=[$POLICIES]"
printf "%-12s %-9s %7s %10s %11s  %s\n" "workload" "policy" "busy" "imbalance" "edges" "compute/host"

for wl in $WORKLOADS; do
  for pol in $POLICIES; do
    sym="$HERE/variants/${wl}__${pol}.symbolic.json"
    cdag="$HERE/cluster_dags/${wl}__${pol}.json"

    python3 "$HERE/gen_variants.py" "$wl" "$pol" --nodes "$NODES" --out "$sym" \
      || die "gen_variants failed for $wl/$pol"

    if ! "$PART" "$sym" --nodes "$NODES" > "$cdag" 2>"$cdag.err"; then
      printf "%-12s %-9s %7s %10s %11s  %s\n" "$wl" "$pol" "ERR" "-" "-" "$(tr -d '\n' < "$cdag.err" | tail -c 80)"
      echo "$wl,$pol,$NODES,ERR,ERR,ERR,partition-failed" >> "$CSV"
      continue
    fi
    rm -f "$cdag.err"

    stats="$(python3 "$HERE/placement_stats.py" "$cdag")"
    busy=$(echo "$stats"   | python3 -c "import json,sys;print(json.load(sys.stdin)['busy_hosts'])")
    imb=$(echo "$stats"    | python3 -c "import json,sys;print(json.load(sys.stdin)['imbalance'])")
    edges=$(echo "$stats"  | python3 -c "import json,sys;print(json.load(sys.stdin)['cross_node_edges'])")
    perhost=$(echo "$stats"| python3 -c "import json,sys;print('|'.join(map(str,json.load(sys.stdin)['compute_per_host'])))")

    printf "%-12s %-9s %7s %10s %11s  %s\n" "$wl" "$pol" "$busy/$NODES" "$imb" "$edges" "$perhost"
    echo "$wl,$pol,$NODES,$busy,$imb,$edges,$perhost" >> "$CSV"
  done
done

log "wrote $CSV"
log "cluster DAGs ready in $HERE/cluster_dags/ — submit them with run_sweep.sh"
