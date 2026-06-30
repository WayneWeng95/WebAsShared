#!/usr/bin/env bash
# stream_scaling.sh — MediaReview streaming strong & weak scaling, 16→144 threads.
#
#   MODE=strong : fixed total event stream (STRONG_EVENTS, default 2,000,000) at
#                 every thread point. Ideal = makespan halves when threads double.
#   MODE=weak   : fixed EVENTS_PER_NODE (default 500,000); total = 500k × nodes.
#                 Ideal = makespan flat, aggregate throughput linear in threads.
#
# Thread point T → nodes = T/16, parts = 16 per node → total *_apply workers = T.
# Reuses the inter-node streaming suite's generators (gen_events.py + gen_cluster_dag.py,
# data-parallel + RDMA partial-tally merge on the last node) and submits the
# ClusterDag from the coordinator. No rebuild — same mr_* guest as ../intra-node.
#
# CORRECTNESS: the merger's mr_summary gate writes total_events; each cell must
# report total_events == EVENTS or it is flagged success=false.
#
# PREREQS: cluster up (coordinator + workers), rdma_ucm loaded on every node.
#
# Usage:
#   ./stream_scaling.sh strong [REPEATS]
#   ./stream_scaling.sh weak   [REPEATS]
#   THREADS / STRONG_EVENTS / EVENTS_PER_NODE / USERS / PARTS / CFG env overrides.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SUITE="$ROOT/Tests/Streaming Application_Benchmark"
INTER="$SUITE/inter-node"

MODE="${1:-strong}"
REPEATS="${2:-3}"
case "$MODE" in strong|weak) ;; *) echo "MODE must be strong|weak" >&2; exit 1 ;; esac

WORKLOAD="mediareview"
THREADS="${THREADS:-16 48 96 144}"
THREADS_PER_NODE="${THREADS_PER_NODE:-16}"
PARTS="${PARTS:-$THREADS_PER_NODE}"          # local partitions per node = threads/node
# MediaReview is CAPACITY-bound, not input-size-bound (unlike WordCount, which scaled
# fine from 500MB→2GB). The per-node SHM stream-slot window caps reliable throughput
# at ~0.5M events/node in the multi-node setting — a single node alone tolerates ~1.0M,
# but the cross-node RDMA partial-tally merge adds enough per-node pressure that 0.7M+
# silently DROPS events on some nodes/reps (verified: 0.5M all-exact; 0.7M drops 2/4
# cells; 1.0M drops at nearly every point). So these sizes are the reliable ceiling —
# pushing higher just trips the correctness gate, it does not improve scaling.
#
#   strong: fixed total 800k — the largest a single node holds for an exact 16-executor
#           speedup baseline (1.0M one-shot is exact but flakes across reps).
STRONG_EVENTS="${STRONG_EVENTS:-800000}"
#   weak:   500k events/node — the largest that stays exact on every node once the
#           merge overhead is included.
EVENTS_PER_NODE="${EVENTS_PER_NODE:-500000}"
USERS="${USERS:-10000}"
SKEW="${SKEW:-0.0}"
# Partial-tally merge topology: flat = single-merger N-way fan-in (original);
# tree = hierarchical k-ary reduction (fan-in BRANCH/level, ~log_BRANCH(N) depth) to
# cut the single-merger weak-scaling bottleneck.
MERGE="${MERGE:-flat}"
BRANCH="${BRANCH:-2}"
# AOT=1 runs the precompiled guest.cwasm so each fanned-out apply worker pays a cheap
# mmap instead of a full JIT — the apply fan-out is ~80% of per-node compute and
# ~half of that is JIT (verified single-node: 4004ms JIT → 2140ms AOT).
AOT="${AOT:-0}"
AOT_FLAG=""; [ "$AOT" = "1" ] && AOT_FLAG="--aot"
CFG="${CFG:-$ROOT/NodeAgent/agent_coordinator.toml}"
SETTLE_S="${SETTLE_S:-3}"
# Coordinator metrics log: per-node rss_bytes + shm_bump_offset, tagged by job id.
METRICS_LOG="${METRICS_LOG:-/tmp/node_agent_metrics.jsonl}"

NODE_AGENT="$ROOT/node-agent"; [ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
GEN_EVENTS="$SUITE/intra-node/gen_events.py"
GEN_DAG="$INTER/gen_cluster_dag.py"
DATADIR_REL="TestData/stream_app/cluster"
DATADIR="$ROOT/$DATADIR_REL"
OUT_REL="TestOutput/rdma_stream_${WORKLOAD}.txt"
LOGDIR="$HERE/logs_stream_$MODE"
CSV="$HERE/analysis/results_stream_$MODE.csv"

log() { printf '\033[1;36m[stream-%s]\033[0m %s\n' "$MODE" "$*"; }
die() { printf '\033[1;31m[stream-%s] %s\033[0m\n' "$MODE" "$*" >&2; exit 1; }
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

[ -x "$NODE_AGENT" ] || die "node-agent not built: $NODE_AGENT"
[ -f "$GEN_EVENTS" ] || die "gen_events.py not found: $GEN_EVENTS"
[ -f "$GEN_DAG" ]    || die "gen_cluster_dag.py not found: $GEN_DAG"
[ -f "$CFG" ]        || die "config not found: $CFG"
mkdir -p "$DATADIR" "$ROOT/TestOutput" "$LOGDIR" "$HERE/analysis"

# Generate the full stream and split round-robin into N disjoint per-node slices.
gen_and_split() {
  local events="$1" nodes="$2"
  python3 "$GEN_EVENTS" "$WORKLOAD" --events "$events" --users "$USERS" --skew "$SKEW" > /tmp/.scl_events.csv
  python3 - "$nodes" "$DATADIR" <<'PY'
import sys
nodes, ddir = int(sys.argv[1]), sys.argv[2]
lines = [l for l in open('/tmp/.scl_events.csv') if l.strip() and not l.startswith('#')]
outs = [open(f"{ddir}/events_node{i}.csv", "w") for i in range(nodes)]
for idx, l in enumerate(lines):
    outs[idx % nodes].write(l)
for o in outs: o.close()
print(len(lines))
PY
}

# Peak memory for a job: per-node max(rss_bytes + shm_bump_offset) over METRICS_LOG
# samples tagged with this job_id. echo "<max_mb_per_node> <sum_mb_cluster>".
mem_peak() {
  local job="$1"
  [ -n "$job" ] && [ -f "$METRICS_LOG" ] || { echo "NA NA"; return; }
  JOB="$job" LOG="$METRICS_LOG" python3 - <<'PY'
import json, os
job, peak = os.environ["JOB"], {}
for line in open(os.environ["LOG"]):
    try: d = json.loads(line)
    except Exception: continue
    if d.get("current_job_id") != job: continue
    n = d["node_id"]; tot = d.get("rss_bytes",0) + d.get("shm_bump_offset",0)
    peak[n] = max(peak.get(n,0), tot)
if peak:
    print(f"{max(peak.values())/1048576:.1f} {sum(peak.values())/1048576:.1f}")
else:
    print("NA NA")
PY
}

# Submit a ClusterDag; echo "<wall_ms> <ok> <total_events> <mem_max_mb> <mem_sum_mb>".
submit_run() {
  local dag="$1" runlog="$2" t0 t1 rc wall ok tev job mem
  rm -f "$ROOT/$OUT_REL"
  t0=$(date +%s%N)
  ( cd "$ROOT" && "$NODE_AGENT" submit --config "$CFG" --dag "$dag" $AOT_FLAG ) >"$runlog" 2>&1
  rc=$?; t1=$(date +%s%N)
  wall=$(grep -oiE "total wall time:[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | head -1)
  [ -n "$wall" ] || wall=$(( (t1-t0)/1000000 ))
  ok=1; grep -qiE "Success:[[:space:]]*true" "$runlog" || ok=0; [ $rc -ne 0 ] && ok=0
  tev=$(grep -oiE "total_events[ =:]+[0-9]+" "$ROOT/$OUT_REL" 2>/dev/null | grep -oE "[0-9]+" | head -1)
  [ -n "$tev" ] || tev=NA
  job=$(grep -oE "Job ID:[[:space:]]*job_[0-9]+" "$runlog" | grep -oE "job_[0-9]+" | head -1)
  mem=$(mem_peak "$job")
  echo "$wall $ok $tev $mem"
}

# policy column is "n/a": the streaming path is a hand-authored data-parallel
# ClusterDag (gen_cluster_dag.py), not partitioner-placed — no pack/balanced knob.
echo "workload,mode,policy,threads,nodes,events,events_per_node,wall_ms_median,wall_ms_min,wall_ms_max,throughput_ev_s,speedup,efficiency,peak_mem_mb_per_node,peak_mem_mb_total,total_events,expect,success,reps" > "$CSV"
log "threads=[$THREADS] reps=$REPEATS events=$([ "$MODE" = strong ] && echo "$STRONG_EVENTS (fixed)" || echo "${EVENTS_PER_NODE}/node") parts=$PARTS cfg=$CFG"
printf "%7s %5s %9s %11s %12s %8s %8s %9s %5s\n" "threads" "nodes" "events" "makespan_ms" "events/s" "speedup" "eff" "mem_MB/nd" "ok"

T_REF=""; MS_REF=""
for T in $THREADS; do
  nodes=$(( T / THREADS_PER_NODE ))
  [ "$nodes" -ge 1 ] || die "threads=$T < $THREADS_PER_NODE"
  if [ "$MODE" = strong ]; then EVENTS="$STRONG_EVENTS"; else EVENTS=$(( EVENTS_PER_NODE * nodes )); fi
  ev_per_node=$(( EVENTS / nodes ))

  log "gen+split: $EVENTS events → $nodes slices"
  produced=$(gen_and_split "$EVENTS" "$nodes")
  [ "$produced" = "$EVENTS" ] || log "note: gen produced $produced events (gate compares against this)"

  tag="stream__${MODE}__t${T}"
  dag="$INTER/.scaling_${tag}.json"
  python3 "$GEN_DAG" "$WORKLOAD" --nodes "$nodes" --parts "$PARTS" --users "$USERS" \
      --merge "$MERGE" --branch "$BRANCH" \
      --events-prefix "$DATADIR_REL/events_node" --out "$OUT_REL" --persist none > "$dag" || die "gen_cluster_dag failed $tag"

  ms_list=(); ok_all=1; last_tev=NA; mem_max=0; mem_sum=0
  for r in $(seq 1 "$REPEATS"); do
    read -r wall ok tev mmax msum < <(submit_run "$dag" "$LOGDIR/$tag.rep${r}.log")
    [ "$ok" = 1 ] || ok_all=0
    last_tev="$tev"
    [ "$tev" = "$produced" ] || ok_all=0
    ms_list+=("$wall")
    [ "$mmax" != NA ] && mem_max=$(awk "BEGIN{print ($mmax>$mem_max)?$mmax:$mem_max}")
    [ "$msum" != NA ] && mem_sum=$(awk "BEGIN{print ($msum>$mem_sum)?$msum:$mem_sum}")
    sleep "$SETTLE_S"
  done

  med=$(median "${ms_list[@]}")
  mn=$(printf '%s\n' "${ms_list[@]}" | sort -n | head -1)
  mx=$(printf '%s\n' "${ms_list[@]}" | sort -n | tail -1)
  evps=$(awk "BEGIN{ if ($med>0) printf \"%.0f\", $produced/($med/1000); else print \"NA\" }")
  [ -n "$T_REF" ] || { T_REF="$T"; MS_REF="$med"; }
  speedup=$(awk "BEGIN{ if ($med>0) printf \"%.2f\", $MS_REF/$med; else print \"NA\" }")
  if [ "$MODE" = strong ]; then
    efficiency=$(awk "BEGIN{ if ($med>0) printf \"%.2f\", ($MS_REF/$med)*$T_REF/$T; else print \"NA\" }")
  else
    efficiency=$(awk "BEGIN{ if ($med>0) printf \"%.2f\", $MS_REF/$med; else print \"NA\" }")
  fi
  okstr=$([ "$ok_all" = 1 ] && echo true || echo false)

  printf "%7s %5s %9s %11s %12s %8s %8s %9s %5s\n" "$T" "$nodes" "$produced" "$med" "$evps" "$speedup" "$efficiency" "$mem_max" "$okstr"
  echo "$WORKLOAD,$MODE,n/a,$T,$nodes,$produced,$ev_per_node,$med,$mn,$mx,$evps,$speedup,$efficiency,$mem_max,$mem_sum,$last_tev,$produced,$okstr,$REPEATS" >> "$CSV"
done

log "wrote $CSV  (logs in $LOGDIR/)"
