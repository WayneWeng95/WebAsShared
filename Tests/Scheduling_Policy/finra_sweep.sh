#!/usr/bin/env bash
# finra_sweep.sh — finra audit across (input size × fanout × placement policy).
#
# Companion to wc_size_sweep.sh, same axes/shape, but finra is the OPPOSITE
# workload story: word_count is bandwidth-bound (balanced wins on makespan by
# using more nodes); finra is COORDINATION-bound (light compute, many transfers)
# so locality/pack tends to win — fewer cross-node hops beat more parallelism.
#
# finra's fan is NOT homogeneous like word_count's map workers — it's 8 FIXED
# audit rules. gen_variants.py's hybrid transform shards only the 5 STATELESS
# per-record rules into S = round((fanout-3)/5) slices each (each scans a disjoint
# 1/S of the trades; merge SUMS the partials → exact total) and keeps the 3
# STATEFUL rules (wash/spoofing/concentration) as single full-data nodes. So
# the stateless shards are tiled so the total fan == fanout exactly (32 → 3 stateful
# + 29 stateless [6,6,6,6,5]; 48 → 3 + 45 [9,9,9,9,9]). With PACK_CAP=16 pack
# consolidates first-fit (32 → 2 nodes [16,16]; 48 → 3 nodes [16,16,16]) while
# balanced spreads across all 4 — matching word_count's node placement.
#
# CORRECTNESS: per size, a single-node ground-truth run establishes the expected
# total_violations (it varies with the trades file); every distributed cell must
# reproduce it EXACTLY or it's flagged success=false (it's wrong, not just slow).
#
# Pre-partitions OFFLINE (embedded policy authoritative — a live cluster's SCX
# hints would otherwise override it), then submits the finished ClusterDag and
# times it. Same submit/timing/median machinery as wc_size_sweep.sh.
#
# Metrics (mirrors analyze_exec.py's makespan-vs-cost split):
#   makespan_ms    end-to-end latency = coordinator "total wall time" (median).
#   total_exec_ms  Σ per-node busy time over WORKING nodes (those running
#                  finra_audit_rule); IDLE nodes (pack's spares) excluded.
#   throughput     trades/s = TRADES / (makespan/1000).
#   violations     correctness gate vs the per-size ground truth.
#
# PREREQUISITES: cluster up (coordinator + workers) with the REBUILT shard-aware
# guest deployed on every node, and TestData/finra/trades_<N>.csv present.
#
# Usage:
#   ./finra_sweep.sh [REPEATS] [CONFIG]
#   SIZES / FANOUTS / POLICIES env override the axes. SETTLE_S between runs.
#
#   ./finra_sweep.sh                       # 3 sizes × {32,48} × {pack,balanced} ×5
#   SIZES=trades_100000 POLICIES=pack ./finra_sweep.sh 3
#
# Then re-derive makespan-vs-cost (the 2nd CSV, like analyze_exec.py for wc):
#   python3 analysis/analyze_finra.py      # → analysis/results_finra_exec.csv
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

REPEATS="${1:-5}"
CONFIG="${2:-$ROOT/NodeAgent/agent.toml}"
NODES="${NODES:-4}"
# Trades files in TestData/finra/, smallest→largest (trade count = filename).
SIZES="${SIZES:-trades_10000 trades_100000 trades_1000000}"
# Requested fanout = effective worker count (3 stateful + 5·S stateless shards).
FANOUTS="${FANOUTS:-32 48}"
# pack vs balanced is the headline contrast (same as wc_size_sweep.sh).
POLICIES="${POLICIES:-pack balanced}"
SETTLE_S="${SETTLE_S:-3}"
# Per-node worker cap driving pack's first-fit consolidation, set to the executor's
# per-node worker limit (16). With exact fanouts (gen_variants tiles the stateless
# rules so total == fanout), pack fits 32 → [16,16] (2 nodes) and 48 → [16,16,16]
# (3 nodes), every node ≤16 — no node exceeds the worker stage-width cap.
PACK_CAP="${PACK_CAP:-16}"

NODE_AGENT="$ROOT/node-agent"
[ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
PART="$ROOT/Partitioner/target/release/partition"
CDIR="$HERE/finra_dags"
LOGDIR="$HERE/logs_finra"
CSV="$HERE/analysis/results_finra.csv"
# Where finra_merge_results flushes its summary (the `save` Output node). Read
# after each run to extract total_violations for the correctness gate.
RESULT_FILE="$ROOT/TestOutput/finra_ap_result.txt"

log() { printf '\033[1;36m[finra]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[finra] %s\033[0m\n' "$*" >&2; exit 1; }
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# Submit a pre-partitioned ClusterDag, echo "<wall_ms> <violations>" (violations
# from the result file, "NA" if absent). $1=cdag, $2=runlog.
submit_run() {
  local cdag="$1" runlog="$2" t0 t1 rc wall viol
  rm -f "$RESULT_FILE"
  t0=$(date +%s%N)
  ( cd "$ROOT" && "$NODE_AGENT" submit --config "$CONFIG" --dag "$cdag" ) >"$runlog" 2>&1
  rc=$?; t1=$(date +%s%N)
  wall=$(grep -oiE "total wall time:[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | head -1)
  [ -n "$wall" ] || wall=$(( (t1-t0)/1000000 ))
  grep -qiE "Success:[[:space:]]*true" "$runlog" || { [ $rc -ne 0 ] && wall=-1; }
  viol=$(grep -oE "total_violations=[0-9]+" "$RESULT_FILE" 2>/dev/null | grep -oE "[0-9]+" | head -1)
  [ -n "$viol" ] || viol="NA"
  echo "$wall $viol"
}

[ -x "$NODE_AGENT" ] || die "node-agent not built: $NODE_AGENT"
[ -x "$PART" ]       || die "partition binary not built: $PART"
[ -f "$CONFIG" ]     || die "config not found: $CONFIG"
mkdir -p "$CDIR" "$LOGDIR"

# makespan_ms = coordinator total wall time; total_exec_ms = Σ busy over working nodes.
echo "trades,corpus,fanout,policy,nodes_used,makespan_ms_median,makespan_ms_min,makespan_ms_max,total_exec_ms_median,throughput_trades_s,violations,expect,success,reps" > "$CSV"
log "nodes=$NODES reps=$REPEATS sizes=[$SIZES] fanouts=[$FANOUTS] policies=[$POLICIES] pack_cap=$PACK_CAP"
printf "%9s %4s %-9s %6s %11s %11s %12s %10s %5s\n" "trades" "fan" "policy" "nodes" "makespan" "total_exec" "trades/s" "violations" "ok"

for size in $SIZES; do
  CORPUS_FILE="$ROOT/TestData/finra/$size.csv"
  if [ ! -f "$CORPUS_FILE" ]; then log "SKIP (missing): TestData/finra/$size.csv"; continue; fi
  trades=$(( $(wc -l < "$CORPUS_FILE") - 1 ))
  rel="TestData/finra/$size.csv"

  # ── Ground truth: single-node, stock 8-rule finra (no sharding) on this file ──
  gt_sym="$CDIR/gt__${size}.sym.json"; gt_cdag="$CDIR/gt__${size}.json"
  python3 "$HERE/gen_variants.py" finra pack --nodes 1 --input "$rel" --out "$gt_sym" || die "gt gen failed $size"
  "$PART" "$gt_sym" --nodes 1 > "$gt_cdag" 2>"$gt_cdag.err" || { cat "$gt_cdag.err"; die "gt partition failed $size"; }
  rm -f "$gt_cdag.err"
  read -r _gtwall EXPECT < <(submit_run "$gt_cdag" "$LOGDIR/gt__${size}.log")
  [ "$EXPECT" != "NA" ] || die "ground-truth run produced no total_violations for $size (see $LOGDIR/gt__${size}.log)"
  log "size=$size trades=$trades ground-truth total_violations=$EXPECT"

  for fo in $FANOUTS; do
  for pol in $POLICIES; do
    tag="finra__${pol}__${size}__f${fo}"
    sym="$CDIR/$tag.sym.json"; cdag="$CDIR/$tag.json"
    python3 "$HERE/gen_variants.py" finra "$pol" --nodes "$NODES" \
      --input "$rel" --fanout "$fo" --pack-cap "$PACK_CAP" --out "$sym" || die "gen failed $tag"
    "$PART" "$sym" --nodes "$NODES" > "$cdag" 2>"$cdag.err" || { cat "$cdag.err"; die "partition failed $tag"; }
    rm -f "$cdag.err"

    # Working node IDs (ran ≥1 audit rule). Idle nodes excluded from total_exec.
    work_ids=$(python3 - "$cdag" <<'PY'
import json,sys
nd=json.load(open(sys.argv[1]))["node_dags"]
work=[k for k in sorted(nd,key=int) if any(isinstance(n.get('kind'),dict) and n['kind'].get('WasmVoid',{}).get('func')=='finra_audit_rule' for n in nd[k])]
print(' '.join(work))
PY
)
    nodes_used=$(echo $work_ids | wc -w)

    ms_list=(); exec_list=(); ok_all=1; last_viol="NA"
    for r in $(seq 1 "$REPEATS"); do
      runlog="$LOGDIR/$tag.rep${r}.log"
      read -r wall viol < <(submit_run "$cdag" "$runlog")
      [ "$wall" = "-1" ] && { ok_all=0; wall=0; }
      last_viol="$viol"
      [ "$viol" = "$EXPECT" ] || ok_all=0
      texec=$(WORK="$work_ids" python3 - "$runlog" <<'PY'
import os,re,sys
work=set(os.environ.get("WORK","").split())
text=open(sys.argv[1]).read()
times={n:int(ms) for n,ms in re.findall(r"node\s+(\d+)\s+\([^)]*\):\s+(\d+)ms",text,re.I)}
print(sum(t for n,t in times.items() if n in work) if times else 0)
PY
)
      ms_list+=("$wall"); exec_list+=("$texec")
      sleep "$SETTLE_S"
    done

    med=$(median "${ms_list[@]}")
    mn=$(printf '%s\n' "${ms_list[@]}" | sort -n | head -1)
    mx=$(printf '%s\n' "${ms_list[@]}" | sort -n | tail -1)
    emed=$(median "${exec_list[@]}")
    tps=$(awk "BEGIN{ if ($med>0) printf \"%.0f\", $trades/($med/1000); else print \"NA\" }")
    okstr=$([ "$ok_all" = 1 ] && echo true || echo false)

    printf "%9s %4s %-9s %6s %11s %11s %12s %10s %5s\n" "$trades" "$fo" "$pol" "$nodes_used" "$med" "$emed" "$tps" "$last_viol" "$okstr"
    echo "$trades,$size,$fo,$pol,$nodes_used,$med,$mn,$mx,$emed,$tps,$last_viol,$EXPECT,$okstr,$REPEATS" >> "$CSV"
  done
  done
done

log "wrote $CSV  (logs in $LOGDIR/)"
