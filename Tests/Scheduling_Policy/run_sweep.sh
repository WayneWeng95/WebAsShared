#!/usr/bin/env bash
# run_sweep.sh — submit every pre-partitioned (workload × policy) ClusterDag to the
# live 4-node cluster, measure end-to-end latency, and write results.csv.
#
# PREREQUISITES (the cluster half — do these yourself first):
#   1. Same commit + ./build.sh on all 4 nodes.
#   2. node-agent running on every node with the shared config:
#        node-0 (coordinator, 10.10.1.2):  ./node-agent start --config NodeAgent/agent.toml
#        node-1 (10.10.1.1) ... node-3 (10.10.1.4): same command (auto-detects role)
#   3. Each workload's input data staged where its DAG expects it (the coordinator
#      RDMA-stages shared_inputs to workers, so inputs live on node-0). See README.
#   4. ./prepartition.sh has been run (creates cluster_dags/).
#
# This script does NOT start the cluster. It only submits jobs to the coordinator.
#
# Metric: end-to-end wall-clock measured around `node-agent submit` (submit →
# JobResult), which is the user-visible latency the placement policy moves. The
# coordinator also prints per-stage "[DAG][timing] TOTAL compute" lines on its
# stdout; capture those separately if you want compute-only timing.
#
# Usage:
#   ./run_sweep.sh [REPEATS] [CONFIG]
#   REPEATS  repeats per cell (default 3); reported latency is the MEDIAN.
#   CONFIG   node-agent config (default NodeAgent/agent.toml).
#
#   # subset via env:
#   WORKLOADS="finra ml_training" POLICIES="pack balanced" ./run_sweep.sh 5
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

REPEATS="${1:-3}"
CONFIG="${2:-$ROOT/NodeAgent/agent.toml}"
WORKLOADS="${WORKLOADS:-word_count finra ml_training}"
POLICIES="${POLICIES:-pack balanced spread random}"
# Settle time (seconds) between consecutive submits, so one run's SHM/RDMA
# teardown doesn't bleed into the next. Override with SETTLE_S=N.
SETTLE_S="${SETTLE_S:-3}"

NODE_AGENT="$ROOT/node-agent"
[ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
CSV="$HERE/results.csv"
LOGDIR="$HERE/logs"

log() { printf '\033[1;36m[sweep]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[sweep] %s\033[0m\n' "$*" >&2; exit 1; }
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

[ -x "$NODE_AGENT" ] || die "node-agent not built (run ./build.sh): $NODE_AGENT"
[ -f "$CONFIG" ]     || die "config not found: $CONFIG"
[ -d "$HERE/cluster_dags" ] || die "cluster_dags/ missing — run ./prepartition.sh first"

mkdir -p "$LOGDIR"
# wall_ms      = coordinator-reported "total wall time" (excludes client submit
#                overhead); falls back to externally-measured wall if absent.
# maxcompute_ms= slowest per-node compute from the job Summary (straggler).
echo "workload,policy,wall_ms_median,wall_ms_min,wall_ms_max,maxcompute_ms_median,success,reps" > "$CSV"
log "config=$CONFIG  reps=$REPEATS  workloads=[$WORKLOADS]  policies=[$POLICIES]"
printf "%-12s %-9s %12s %14s %8s %6s\n" "workload" "policy" "wall_ms" "maxcompute_ms" "ok" "reps"

for wl in $WORKLOADS; do
  for pol in $POLICIES; do
    cdag="$HERE/cluster_dags/${wl}__${pol}.json"
    if [ ! -f "$cdag" ]; then
      log "SKIP (missing cluster dag): ${wl}__${pol}"
      continue
    fi

    ms_list=(); comp_list=(); ok_all=1
    for r in $(seq 1 "$REPEATS"); do
      runlog="$LOGDIR/${wl}__${pol}.rep${r}.log"
      t0=$(date +%s%N)
      ( cd "$ROOT" && "$NODE_AGENT" submit --config "$CONFIG" --dag "$cdag" ) >"$runlog" 2>&1
      rc=$?
      t1=$(date +%s%N)
      ext_ms=$(( (t1 - t0) / 1000000 ))

      ok=0
      grep -qiE "Success:[[:space:]]*true" "$runlog" && ok=1
      [ $rc -ne 0 ] && ok=0
      [ "$ok" = 1 ] || ok_all=0

      # Prefer the coordinator's "total wall time: NNNms"; fall back to external.
      wall=$(grep -oiE "total wall time:[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | head -1)
      [ -n "$wall" ] || wall="$ext_ms"
      # Slowest per-node compute ("node K (...): NNNms") = straggler.
      comp=$(grep -oiE "\):[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | sort -n | tail -1)
      [ -n "$comp" ] || comp=0

      ms_list+=("$wall"); comp_list+=("$comp")
      sleep "$SETTLE_S"   # let the cluster quiesce before the next submit
    done

    med=$(median "${ms_list[@]}")
    mn=$(printf '%s\n' "${ms_list[@]}" | sort -n | head -1)
    mx=$(printf '%s\n' "${ms_list[@]}" | sort -n | tail -1)
    cmed=$(median "${comp_list[@]}")
    okstr=$([ "$ok_all" = 1 ] && echo true || echo false)

    printf "%-12s %-9s %12s %14s %8s %6s\n" "$wl" "$pol" "$med" "$cmed" "$okstr" "$REPEATS"
    echo "$wl,$pol,$med,$mn,$mx,$cmed,$okstr,$REPEATS" >> "$CSV"
  done
done

log "wrote $CSV  (per-run logs in $LOGDIR/)"
log "now run: python3 analyze.py"
