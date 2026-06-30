#!/usr/bin/env bash
# stream_strong_batched.sh — MediaReview STRONG scaling via BATCHED processing.
#
# Strong scaling needs a FIXED total, but MediaReview's per-node SHM stream-slot
# window caps a single shot at ~0.5M events/node — so a large total can't fit on
# few nodes. Batching fixes that: each node processes its TOTAL/N slice in B
# SEQUENTIAL batches of ~BATCH_TARGET events; slots are freed between batches so
# peak SHM = one batch, while the keyed state + appended tallies accumulate (same
# result). B shrinks as nodes grow (B = round((TOTAL/N)/BATCH_TARGET)), so adding
# nodes does fewer batches/node → strong-scaling speedup, all at ~constant per-node
# memory.
#
#   TOTAL=3.6M, BATCH_TARGET=300k →  nodes  1   3   6   9
#                                    batches 12  4   2   1   (per node, ~300k each)
#
# Usage:  ./stream_strong_batched.sh [REPEATS]
#   THREADS / TOTAL / BATCH_TARGET / MERGE / BRANCH / CFG env overrides.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SUITE="$ROOT/Tests/Streaming Application_Benchmark"
INTER="$SUITE/inter-node"

REPEATS="${1:-3}"
WORKLOAD="mediareview"
THREADS="${THREADS:-16 48 96 144}"
THREADS_PER_NODE="${THREADS_PER_NODE:-16}"
PARTS="${PARTS:-$THREADS_PER_NODE}"
TOTAL="${TOTAL:-3600000}"          # fixed total events (strong scaling)
BATCH_TARGET="${BATCH_TARGET:-300000}"  # ~events per node per batch
USERS="${USERS:-10000}"; SKEW="${SKEW:-0.0}"
MERGE="${MERGE:-flat}"; BRANCH="${BRANCH:-2}"
CFG="${CFG:-$ROOT/NodeAgent/agent_coordinator.toml}"
SETTLE_S="${SETTLE_S:-3}"
METRICS_LOG="${METRICS_LOG:-/tmp/node_agent_metrics.jsonl}"

NODE_AGENT="$ROOT/node-agent"; [ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
GEN_EVENTS="$SUITE/intra-node/gen_events.py"
GEN_DAG="$INTER/gen_cluster_dag.py"
DATADIR_REL="TestData/stream_app/cluster"; DATADIR="$ROOT/$DATADIR_REL"
OUT_REL="TestOutput/rdma_stream_${WORKLOAD}.txt"
LOGDIR="$HERE/logs_stream_strong_batched"; CSV="$HERE/analysis/results_stream_strong_batched.csv"

log() { printf '\033[1;36m[batched]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[batched] %s\033[0m\n' "$*" >&2; exit 1; }
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

[ -x "$NODE_AGENT" ] || die "node-agent not built"
mkdir -p "$DATADIR" "$ROOT/TestOutput" "$LOGDIR" "$HERE/analysis"

# Generate TOTAL events, split round-robin into N node slices, then split each node
# slice into B contiguous batch files: events_node<i>_b<k>.csv (B=1 → events_node<i>.csv).
gen_split_batch() {  # $1=total $2=nodes $3=batches → echoes produced line count
  python3 "$GEN_EVENTS" "$WORKLOAD" --events "$1" --users "$USERS" --skew "$SKEW" > /tmp/.sb_events.csv
  python3 - "$2" "$3" "$DATADIR" "${args_prefix:-events_node}" <<'PY'
import sys
nodes, batches, ddir, pref = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], sys.argv[4]
lines = [l for l in open('/tmp/.sb_events.csv') if l.strip() and not l.startswith('#')]
# round-robin → N disjoint node slices
slices = [[] for _ in range(nodes)]
for idx, l in enumerate(lines):
    slices[idx % nodes].append(l)
for i in range(nodes):
    if batches <= 1:
        open(f"{ddir}/{pref}{i}.csv", "w").writelines(slices[i])
    else:
        s = slices[i]; per = (len(s) + batches - 1) // batches  # ceil split into B chunks
        for k in range(batches):
            open(f"{ddir}/{pref}{i}_b{k}.csv", "w").writelines(s[k*per:(k+1)*per])
print(len(lines))
PY
}

submit_run() {  # $1=dag $2=runlog → "<wall_ms> <ok> <total_events> <mem_max> <mem_sum>"
  local dag="$1" runlog="$2" t0 t1 rc wall ok tev job mem
  rm -f "$ROOT/$OUT_REL"; t0=$(date +%s%N)
  ( cd "$ROOT" && "$NODE_AGENT" submit --config "$CFG" --dag "$dag" ) >"$runlog" 2>&1
  rc=$?; t1=$(date +%s%N)
  wall=$(grep -oiE "total wall time:[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | head -1)
  [ -n "$wall" ] || wall=$(( (t1-t0)/1000000 ))
  ok=1; grep -qiE "Success:[[:space:]]*true" "$runlog" || ok=0; [ $rc -ne 0 ] && ok=0
  tev=$(grep -oiE "total_events[ =:]+[0-9]+" "$ROOT/$OUT_REL" 2>/dev/null | grep -oE "[0-9]+" | head -1); [ -n "$tev" ] || tev=NA
  job=$(grep -oE "Job ID:[[:space:]]*job_[0-9]+" "$runlog" | grep -oE "job_[0-9]+" | head -1)
  mem=$(JOB="$job" LOG="$METRICS_LOG" python3 - <<'PY'
import json,os
job,peak=os.environ.get("JOB"),{}
if job and os.path.exists(os.environ["LOG"]):
    for line in open(os.environ["LOG"]):
        try: d=json.loads(line)
        except Exception: continue
        if d.get("current_job_id")!=job: continue
        nn=d["node_id"]; peak[nn]=max(peak.get(nn,0), d.get("rss_bytes",0)+d.get("shm_bump_offset",0))
print(f"{max(peak.values())/1048576:.1f} {sum(peak.values())/1048576:.1f}" if peak else "NA NA")
PY
)
  echo "$wall $ok $tev $mem"
}

echo "workload,mode,threads,nodes,batches,total,per_node,per_batch,wall_ms_median,wall_ms_min,wall_ms_max,throughput_ev_s,speedup,peak_mem_mb_per_node,total_events,expect,success,reps" > "$CSV"
log "TOTAL=$TOTAL batch_target=$BATCH_TARGET threads=[$THREADS] merge=$MERGE reps=$REPEATS"
printf "%7s %5s %7s %9s %9s %11s %12s %8s %9s %5s\n" "threads" "nodes" "batches" "per_node" "per_batch" "makespan_ms" "events/s" "speedup" "mem_MB/nd" "ok"

MS_REF=""
for T in $THREADS; do
  nodes=$(( T / THREADS_PER_NODE )); [ "$nodes" -ge 1 ] || die "threads=$T too small"
  per_node=$(( TOTAL / nodes ))
  B=$(awk "BEGIN{b=int(($per_node/$BATCH_TARGET)+0.5); print (b<1)?1:b}")
  per_batch=$(( per_node / B ))

  produced=$(gen_split_batch "$TOTAL" "$nodes" "$B")
  dag="$INTER/.batched_t${T}.json"
  python3 "$GEN_DAG" "$WORKLOAD" --nodes "$nodes" --parts "$PARTS" --users "$USERS" \
      --batches "$B" --merge "$MERGE" --branch "$BRANCH" \
      --events-prefix "$DATADIR_REL/events_node" --out "$OUT_REL" > "$dag" || die "gen_cluster_dag failed t$T"

  ms_list=(); ok_all=1; last_tev=NA; mem_max=0
  for r in $(seq 1 "$REPEATS"); do
    read -r wall ok tev mmax msum < <(submit_run "$dag" "$LOGDIR/t${T}.rep${r}.log")
    [ "$ok" = 1 ] || ok_all=0; last_tev="$tev"; [ "$tev" = "$produced" ] || ok_all=0
    ms_list+=("$wall"); [ "$mmax" != NA ] && mem_max=$(awk "BEGIN{print ($mmax>$mem_max)?$mmax:$mem_max}")
    sleep "$SETTLE_S"
  done
  med=$(median "${ms_list[@]}"); mn=$(printf '%s\n' "${ms_list[@]}"|sort -n|head -1); mx=$(printf '%s\n' "${ms_list[@]}"|sort -n|tail -1)
  tps=$(awk "BEGIN{ if($med>0) printf \"%.0f\", $produced/($med/1000); else print \"NA\" }")
  [ -n "$MS_REF" ] || MS_REF="$med"
  sp=$(awk "BEGIN{ if($med>0) printf \"%.2f\", $MS_REF/$med; else print \"NA\" }")
  okstr=$([ "$ok_all" = 1 ] && echo true || echo false)

  printf "%7s %5s %7s %9s %9s %11s %12s %8s %9s %5s\n" "$T" "$nodes" "$B" "$per_node" "$per_batch" "$med" "$tps" "$sp" "$mem_max" "$okstr"
  echo "$WORKLOAD,strong_batched,$T,$nodes,$B,$TOTAL,$per_node,$per_batch,$med,$mn,$mx,$tps,$sp,$mem_max,$last_tev,$produced,$okstr,$REPEATS" >> "$CSV"
done
log "wrote $CSV  (logs in $LOGDIR/)"
