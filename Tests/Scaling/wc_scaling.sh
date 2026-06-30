#!/usr/bin/env bash
# wc_scaling.sh — WordCount strong & weak scaling, 16→144 threads (1→9 nodes).
#
#   MODE=strong : fixed total corpus (corpus_xlarge.txt, 500 MB) at every thread
#                 point. Ideal = makespan halves when threads double.
#   MODE=weak   : fixed BASE_MB per node (256 MB); total = 256 × nodes, read from
#                 the pre-cut TestData/scaling/corpus_weak_<total>mb.txt (prep_corpora.sh).
#                 Ideal = makespan stays flat as threads grow.
#
# Each thread point T maps to nodes = T/16 and a balanced map fanout = T with
# PACK_CAP=16 → exactly 16 map workers per node (balanced is the placement-
# insensitive control from ../Scheduling_Policy). Reuses that suite's gen_variants.py
# + the partition/submit/median machinery (no rebuild).
#
# CORRECTNESS: per distinct input, a single-node ground-truth run fixes the
# expected reduced word-count signature (sort | md5 of the result file). Every
# distributed cell must reproduce it exactly or it is flagged success=false.
#
# Usage:
#   ./wc_scaling.sh strong [REPEATS]
#   ./wc_scaling.sh weak   [REPEATS]
#   THREADS="16 48 96 144"  override the scaling points (multiples of 16).
#   STRONG_CORPUS / BASE_MB / CONFIG env overrides.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SCHED="$ROOT/Tests/Scheduling_Policy"        # reuse its gen_variants.py

MODE="${1:-strong}"
REPEATS="${2:-3}"
case "$MODE" in strong|weak) ;; *) echo "MODE must be strong|weak" >&2; exit 1 ;; esac

THREADS="${THREADS:-16 48 96 144}"
THREADS_PER_NODE="${THREADS_PER_NODE:-16}"
# Partitioner placement policies to compare. In this node-scaling design fanout =
# 16×nodes exactly fills every node to the pack-cap, so pack and balanced place
# IDENTICALLY (WordCount's heavy stages are placement:"all" — the policy-insensitive
# control). Running both confirms the curves overlap; diverging would flag noise.
POLICIES="${POLICIES:-balanced pack}"
# Node count for the correctness REFERENCE run. Default 1 = single-node ground truth.
# Set >1 when the fixed input is too large to load on one node (e.g. 4 GiB exceeds
# the ~3.48 GiB per-node SHM window) — the reference then runs on GT_NODES nodes and
# the gate becomes a cross-config consistency check (the data-parallel algorithm is
# already proven exact vs single-node by the smaller sweeps).
GT_NODES="${GT_NODES:-1}"
# Fixed total for strong scaling = 2 GiB. Large enough to keep 144 workers fed
# (~14 MB/worker) so the per-node fixed cost (~3.8 s of WASM JIT/spawn) is amortized
# and speedup stays near-linear; small enough to still load on ONE node (16t baseline,
# ~5.6 GB peak RSS+SHM, under the 3.48 GiB SHM window). 4 GiB overflows the 1-node
# window and crashes the coordinator at low node counts — 2 GiB is the safe ceiling.
STRONG_CORPUS="${STRONG_CORPUS:-scaling/corpus_strong_2048mb.txt}"
BASE_MB="${BASE_MB:-256}"                              # per-node bytes for weak
CONFIG="${CONFIG:-$ROOT/NodeAgent/agent.toml}"
SETTLE_S="${SETTLE_S:-3}"
# Coordinator metrics log (per-node rss_bytes + shm_bump_offset samples, tagged by
# current_job_id). Per-node total memory = rss_bytes + shm_bump_offset (README
# "Memory metric fix"). Override if agent_coordinator.toml uses a different log_path.
METRICS_LOG="${METRICS_LOG:-/tmp/node_agent_metrics.jsonl}"

NODE_AGENT="$ROOT/node-agent"; [ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
PART="$ROOT/Partitioner/target/release/partition"
GENV="$SCHED/gen_variants.py"
CDIR="$HERE/cluster_dags"
LOGDIR="$HERE/logs_wc_$MODE"
CSV="$HERE/analysis/results_wc_$MODE.csv"
RESULT_FILE="$ROOT/TestOutput/wc_auto_placement_result.txt"

log() { printf '\033[1;36m[wc-%s]\033[0m %s\n' "$MODE" "$*"; }
die() { printf '\033[1;31m[wc-%s] %s\033[0m\n' "$MODE" "$*" >&2; exit 1; }
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

[ -x "$NODE_AGENT" ] || die "node-agent not built: $NODE_AGENT"
[ -x "$PART" ]       || die "partition binary not built: $PART"
[ -f "$GENV" ]       || die "gen_variants.py not found: $GENV"
[ -f "$CONFIG" ]     || die "config not found: $CONFIG"
mkdir -p "$CDIR" "$LOGDIR" "$HERE/analysis"

# input file (relative to ROOT) for a given node count
input_rel() {
  local nodes="$1"
  if [ "$MODE" = strong ]; then
    echo "TestData/$STRONG_CORPUS"
  else
    echo "TestData/scaling/corpus_weak_$(( BASE_MB * nodes ))mb.txt"
  fi
}

# Order-independent correctness signature of the reduced result file. Excludes the
# `map_records_received=` diagnostic header — it counts map invocations and so
# legitimately scales with fanout/node count; the actual word→count rows,
# unique_words, and total_occurrences are what must match the ground truth.
sig() { [ -f "$RESULT_FILE" ] && grep -v '^map_records_received=' "$RESULT_FILE" | sort | md5sum | cut -d' ' -f1 || echo NA; }

# Peak memory for a job: parse METRICS_LOG for samples tagged with this job_id,
# take per-node max(rss_bytes + shm_bump_offset). echo "<max_mb_per_node> <sum_mb_cluster>".
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

# Submit a pre-partitioned ClusterDag; echo "<wall_ms> <ok> <sig> <mem_max_mb> <mem_sum_mb>".
submit_run() {
  local cdag="$1" runlog="$2" t0 t1 rc wall ok job mem
  rm -f "$RESULT_FILE"
  t0=$(date +%s%N)
  ( cd "$ROOT" && "$NODE_AGENT" submit --config "$CONFIG" --dag "$cdag" ) >"$runlog" 2>&1
  rc=$?; t1=$(date +%s%N)
  wall=$(grep -oiE "total wall time:[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | head -1)
  [ -n "$wall" ] || wall=$(( (t1-t0)/1000000 ))
  ok=1; grep -qiE "Success:[[:space:]]*true" "$runlog" || ok=0; [ $rc -ne 0 ] && ok=0
  job=$(grep -oE "Job ID:[[:space:]]*job_[0-9]+" "$runlog" | grep -oE "job_[0-9]+" | head -1)
  mem=$(mem_peak "$job")
  echo "$wall $ok $(sig) $mem"
}

echo "workload,mode,policy,threads,nodes,total_mb,per_thread_mb,wall_ms_median,wall_ms_min,wall_ms_max,throughput_mb_s,speedup,efficiency,peak_mem_mb_per_node,peak_mem_mb_total,success,reps" > "$CSV"
log "threads=[$THREADS] policies=[$POLICIES] reps=$REPEATS corpus=$([ "$MODE" = strong ] && echo "$STRONG_CORPUS (fixed)" || echo "${BASE_MB}MB/node") config=$CONFIG"
printf "%-9s %7s %5s %8s %11s %10s %8s %8s %9s %5s\n" "policy" "threads" "nodes" "total_MB" "makespan_ms" "MB/s" "speedup" "eff" "mem_MB/nd" "ok"

declare -A GT_SIG=()       # ground-truth signature per input file (policy-independent)
declare -A MS_REF=()       # per-policy makespan at the smallest thread point (speedup baseline)

for pol in $POLICIES; do
for T in $THREADS; do
  nodes=$(( T / THREADS_PER_NODE ))
  [ "$nodes" -ge 1 ] || die "threads=$T < $THREADS_PER_NODE (one node)"
  rel="$(input_rel "$nodes")"
  [ -f "$ROOT/$rel" ] || { log "SKIP threads=$T: missing $rel (run prep_corpora.sh?)"; continue; }
  bytes=$(wc -c < "$ROOT/$rel"); total_mb=$(awk "BEGIN{printf \"%.0f\", $bytes/1048576}")
  per_thread_mb=$(awk "BEGIN{printf \"%.1f\", $total_mb/$T}")

  # ── correctness reference for this input (GT_NODES nodes, fanout 16×GT_NODES) ──
  if [ -z "${GT_SIG[$rel]:-}" ]; then
    gt_fanout=$(( 16 * GT_NODES ))
    gt_sym="$CDIR/gt__$(basename "$rel").sym.json"; gt_cdag="$CDIR/gt__$(basename "$rel").json"
    python3 "$GENV" word_count balanced --nodes "$GT_NODES" --input "$rel" --fanout "$gt_fanout" --pack-cap 16 --out "$gt_sym" || die "gt gen failed ($rel)"
    "$PART" "$gt_sym" --nodes "$GT_NODES" > "$gt_cdag" 2>"$gt_cdag.err" || { cat "$gt_cdag.err"; die "gt partition failed ($rel)"; }
    rm -f "$gt_cdag.err"
    read -r _w _ok gsig _gmax _gsum < <(submit_run "$gt_cdag" "$LOGDIR/gt__$(basename "$rel").log")
    [ "$gsig" != NA ] || die "reference run produced no result file for $rel (GT_NODES=$GT_NODES too few? input too large per node?)"
    GT_SIG[$rel]="$gsig"
    log "reference ($GT_NODES-node) $rel sig=$gsig"
  fi
  EXPECT="${GT_SIG[$rel]}"

  # ── distributed cell: nodes=$nodes, policy=$pol, fanout=$T, pack_cap=16 ──
  tag="wc__${MODE}__${pol}__t${T}"
  sym="$CDIR/$tag.symbolic.json"; cdag="$CDIR/$tag.json"
  python3 "$GENV" word_count "$pol" --nodes "$nodes" --input "$rel" \
      --fanout "$T" --pack-cap "$THREADS_PER_NODE" --out "$sym" || die "gen failed $tag"
  "$PART" "$sym" --nodes "$nodes" > "$cdag" 2>"$cdag.err" || { cat "$cdag.err"; die "partition failed $tag"; }
  rm -f "$cdag.err"

  ms_list=(); ok_all=1; mem_max=0; mem_sum=0
  for r in $(seq 1 "$REPEATS"); do
    read -r wall ok csig mmax msum < <(submit_run "$cdag" "$LOGDIR/$tag.rep${r}.log")
    [ "$ok" = 1 ] || ok_all=0
    [ "$csig" = "$EXPECT" ] || ok_all=0
    ms_list+=("$wall")
    # memory peak = worst (max) over reps, per-node and cluster-total
    [ "$mmax" != NA ] && mem_max=$(awk "BEGIN{print ($mmax>$mem_max)?$mmax:$mem_max}")
    [ "$msum" != NA ] && mem_sum=$(awk "BEGIN{print ($msum>$mem_sum)?$msum:$mem_sum}")
    sleep "$SETTLE_S"
  done

  med=$(median "${ms_list[@]}")
  mn=$(printf '%s\n' "${ms_list[@]}" | sort -n | head -1)
  mx=$(printf '%s\n' "${ms_list[@]}" | sort -n | tail -1)
  mbps=$(awk "BEGIN{ if ($med>0) printf \"%.1f\", $total_mb/($med/1000); else print \"NA\" }")
  # per-policy speedup baseline = that policy's smallest-thread makespan
  [ -n "${MS_REF[$pol]:-}" ] || MS_REF[$pol]="$med"
  ref="${MS_REF[$pol]}"; ref_t=$(echo $THREADS | awk '{print $1}')
  # strong: speedup = T_ref_time/T_time, ideal-linear efficiency vs thread ratio.
  # weak:   speedup col = throughput-scaling = T_ref_time/T_time; efficiency = makespan flatness.
  speedup=$(awk "BEGIN{ if ($med>0) printf \"%.2f\", $ref/$med; else print \"NA\" }")
  if [ "$MODE" = strong ]; then
    efficiency=$(awk "BEGIN{ if ($med>0) printf \"%.2f\", ($ref/$med)*$ref_t/$T; else print \"NA\" }")
  else
    efficiency=$(awk "BEGIN{ if ($med>0) printf \"%.2f\", $ref/$med; else print \"NA\" }")
  fi
  okstr=$([ "$ok_all" = 1 ] && echo true || echo false)

  printf "%-9s %7s %5s %8s %11s %10s %8s %8s %9s %5s\n" "$pol" "$T" "$nodes" "$total_mb" "$med" "$mbps" "$speedup" "$efficiency" "$mem_max" "$okstr"
  echo "word_count,$MODE,$pol,$T,$nodes,$total_mb,$per_thread_mb,$med,$mn,$mx,$mbps,$speedup,$efficiency,$mem_max,$mem_sum,$okstr,$REPEATS" >> "$CSV"
done
done

log "wrote $CSV  (logs in $LOGDIR/)"
