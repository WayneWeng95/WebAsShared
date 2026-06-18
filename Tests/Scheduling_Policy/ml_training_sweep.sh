#!/usr/bin/env bash
# ml_training_sweep.sh — distributed SGD training across (input size × placement policy).
#
# Third workload in the placement-policy study, after word_count (bandwidth-bound,
# the size sweep) and finra (coordination-bound, the locality sweep). ml_training
# is the SGD analogue of finra: a FIXED 8-way fan (train_0..7, one shard each) with
# a heavier per-shard compute (PCA-projected SGD over E epochs) and an iterative
# weight gather. gen_variants.py's generic fan transform already handles it — it
# places the 8 train shards per policy and inserts a PER-MACHINE local aggregate so
# each peer sends exactly ONE combined transfer to node 0 (the local-combine pattern
# that fixed finra's many-to-many deadlock; verified offline: balanced/spread place
# [2,2,2,2] and node 0 sees exactly 3 RemoteRecv — one per peer).
#
# Same fan-out axis as the other two sweeps: fanout = W gradient workers. gen_variants
# regenerates the auto-placement DAG at width W (W encode + W train nodes, each
# encoder co-located with its worker) and places the train_* fan per policy. pack
# consolidates first-fit to PACK_CAP/node (32 → [16,16,0,0]; 48 → [16,16,16,0]);
# balanced/spread spread evenly; random is uneven. Axes: SIZE × FANOUT × POLICY.
#
# CORRECTNESS: per size, a single-node ground-truth run establishes the expected
# weight_checksum (Σ of all model weights after E epochs — fan-out-invariant by the
# gradient-sum argument, see ML_training/README.md); every distributed cell must
# reproduce it EXACTLY or it's flagged success=false (wrong, not just slow). accuracy
# is carried alongside for sanity (deterministic).
#
# Pre-partitions OFFLINE (embedded policy authoritative — a live cluster's SCX hints
# would otherwise override it), then submits the finished ClusterDag and times it.
# Same submit/timing/median machinery as finra_sweep.sh / wc_size_sweep.sh.
#
# Metrics (mirrors analyze_exec.py's makespan-vs-cost split):
#   makespan_ms    end-to-end latency = coordinator "total wall time" (median).
#   total_exec_ms  Σ per-node busy time over WORKING nodes (those running ml_train);
#                  IDLE nodes (pack's spares) excluded.
#   throughput     samples/s = N_samples / (makespan/1000).
#   checksum       correctness gate vs the per-size ground truth.
#
# PREREQUISITES: cluster up (coordinator + workers) with the guest deployed on every
# node, and TestData/ml/<size>.csv present (gen via ML_training/gen_data.py if absent).
#
# Usage:
#   ./ml_training_sweep.sh [REPEATS] [CONFIG]
#   SIZES / POLICIES env override the axes. SETTLE_S between runs.
#
#   ./ml_training_sweep.sh                          # 3 sizes × {pack,balanced} ×5
#   SIZES=sgd_100000 POLICIES="pack balanced spread random" ./ml_training_sweep.sh 3
#
# Then re-derive makespan-vs-cost (the 2nd CSV, like analyze_finra.py):
#   python3 analysis/analyze_ml_training.py         # → analysis/results_ml_training_exec.csv
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

REPEATS="${1:-5}"
CONFIG="${2:-$ROOT/NodeAgent/agent.toml}"
NODES="${NODES:-4}"
# SGD data files in TestData/ml/, smallest→largest (sample count = filename suffix).
SIZES="${SIZES:-sgd_100000 sgd_300000 sgd_600000}"
# Fanout = number of gradient workers W (the SGD fan width), same axis as the other
# two sweeps. gen_variants regenerates the auto-placement DAG at width W and places
# the train_* fan per policy; pack consolidates first-fit to PACK_CAP/node.
FANOUTS="${FANOUTS:-32 48}"
# pack vs balanced is the headline contrast (same as the other sweeps); spread/random
# partition cleanly offline too — add them via POLICIES= to get their live numbers.
POLICIES="${POLICIES:-pack balanced}"
SETTLE_S="${SETTLE_S:-3}"
# Per-node worker cap driving pack's first-fit consolidation (= executor per-node
# worker limit). fanout 32 → pack fills [16,16,0,0] (2 nodes); 48 → [16,16,16,0].
PACK_CAP="${PACK_CAP:-16}"

NODE_AGENT="$ROOT/node-agent"
[ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
PART="$ROOT/Partitioner/target/release/partition"
CDIR="$HERE/ml_training_dags"
LOGDIR="$HERE/logs_ml_training"
CSV="$HERE/analysis/results_ml_training.csv"
# Where the `save` Output node flushes the trained-model summary. Read after each run
# to extract weight_checksum (gate) + accuracy.
RESULT_FILE="$ROOT/TestOutput/ml_training_ap_result.txt"

log() { printf '\033[1;36m[mltrain]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[mltrain] %s\033[0m\n' "$*" >&2; exit 1; }
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# Submit a pre-partitioned ClusterDag, echo "<wall_ms> <checksum> <accuracy>"
# (checksum/accuracy from the result file, "NA" if absent). $1=cdag, $2=runlog.
submit_run() {
  local cdag="$1" runlog="$2" t0 t1 rc wall ck acc
  rm -f "$RESULT_FILE"
  t0=$(date +%s%N)
  ( cd "$ROOT" && "$NODE_AGENT" submit --config "$CONFIG" --dag "$cdag" ) >"$runlog" 2>&1
  rc=$?; t1=$(date +%s%N)
  wall=$(grep -oiE "total wall time:[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | head -1)
  [ -n "$wall" ] || wall=$(( (t1-t0)/1000000 ))
  grep -qiE "Success:[[:space:]]*true" "$runlog" || { [ $rc -ne 0 ] && wall=-1; }
  ck=$(grep -oE "weight_checksum=-?[0-9]+" "$RESULT_FILE" 2>/dev/null | grep -oE "\-?[0-9]+" | head -1)
  acc=$(grep -oE "accuracy=[0-9.]+" "$RESULT_FILE" 2>/dev/null | grep -oE "[0-9.]+" | head -1)
  [ -n "$ck" ]  || ck="NA"
  [ -n "$acc" ] || acc="NA"
  echo "$wall $ck $acc"
}

[ -x "$NODE_AGENT" ] || die "node-agent not built: $NODE_AGENT"
[ -x "$PART" ]       || die "partition binary not built: $PART"
[ -f "$CONFIG" ]     || die "config not found: $CONFIG"
mkdir -p "$CDIR" "$LOGDIR"

# makespan_ms = coordinator total wall time; total_exec_ms = Σ busy over working nodes.
echo "samples,corpus,fanout,policy,nodes_used,makespan_ms_median,makespan_ms_min,makespan_ms_max,total_exec_ms_median,throughput_samples_s,checksum,expect,accuracy,success,reps" > "$CSV"
log "nodes=$NODES reps=$REPEATS sizes=[$SIZES] fanouts=[$FANOUTS] policies=[$POLICIES] pack_cap=$PACK_CAP"
printf "%9s %4s %-9s %6s %11s %11s %12s %12s %7s %5s\n" "samples" "fan" "policy" "nodes" "makespan" "total_exec" "samples/s" "checksum" "acc%" "ok"

for size in $SIZES; do
  CORPUS_FILE="$ROOT/TestData/ml/$size.csv"
  if [ ! -f "$CORPUS_FILE" ]; then log "SKIP (missing): TestData/ml/$size.csv"; continue; fi
  samples=$(( $(wc -l < "$CORPUS_FILE") - 1 ))
  rel="TestData/ml/$size.csv"

  # ── Ground truth: single-node SGD on this file (no distribution). The weight
  # checksum is fan-out-invariant (associative gradient sum + one central step), so
  # one gt run is the gate reference for every fanout/policy cell at this size. ──
  gt_sym="$CDIR/gt__${size}.sym.json"; gt_cdag="$CDIR/gt__${size}.json"
  python3 "$HERE/gen_variants.py" ml_training pack --nodes 1 --input "$rel" --out "$gt_sym" || die "gt gen failed $size"
  "$PART" "$gt_sym" --nodes 1 > "$gt_cdag" 2>"$gt_cdag.err" || { cat "$gt_cdag.err"; die "gt partition failed $size"; }
  rm -f "$gt_cdag.err"
  read -r _gtwall EXPECT _gtacc < <(submit_run "$gt_cdag" "$LOGDIR/gt__${size}.log")
  [ "$EXPECT" != "NA" ] || die "ground-truth run produced no weight_checksum for $size (see $LOGDIR/gt__${size}.log)"
  log "size=$size samples=$samples ground-truth weight_checksum=$EXPECT"

  for fo in $FANOUTS; do
  for pol in $POLICIES; do
    tag="ml_training__${pol}__${size}__f${fo}"
    sym="$CDIR/$tag.sym.json"; cdag="$CDIR/$tag.json"
    python3 "$HERE/gen_variants.py" ml_training "$pol" --nodes "$NODES" \
      --input "$rel" --fanout "$fo" --pack-cap "$PACK_CAP" --out "$sym" || die "gen failed $tag"
    "$PART" "$sym" --nodes "$NODES" > "$cdag" 2>"$cdag.err" || { cat "$cdag.err"; die "partition failed $tag"; }
    rm -f "$cdag.err"

    # Working node IDs (ran ≥1 gradient shard). Idle nodes excluded from total_exec.
    # The SGD fan workers run `sgd_grad` (the gradient kernel).
    work_ids=$(python3 - "$cdag" <<'PY'
import json,sys
nd=json.load(open(sys.argv[1]))["node_dags"]
work=[k for k in sorted(nd,key=int) if any(isinstance(n.get('kind'),dict) and n['kind'].get('WasmVoid',{}).get('func')=='sgd_grad' for n in nd[k])]
print(' '.join(work))
PY
)
    nodes_used=$(echo $work_ids | wc -w)

    ms_list=(); exec_list=(); ok_all=1; last_ck="NA"; last_acc="NA"
    for r in $(seq 1 "$REPEATS"); do
      runlog="$LOGDIR/$tag.rep${r}.log"
      read -r wall ck acc < <(submit_run "$cdag" "$runlog")
      [ "$wall" = "-1" ] && { ok_all=0; wall=0; }
      last_ck="$ck"; last_acc="$acc"
      [ "$ck" = "$EXPECT" ] || ok_all=0
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
    sps=$(awk "BEGIN{ if ($med>0) printf \"%.0f\", $samples/($med/1000); else print \"NA\" }")
    okstr=$([ "$ok_all" = 1 ] && echo true || echo false)

    printf "%9s %4s %-9s %6s %11s %11s %12s %12s %7s %5s\n" "$samples" "$fo" "$pol" "$nodes_used" "$med" "$emed" "$sps" "$last_ck" "$last_acc" "$okstr"
    echo "$samples,$size,$fo,$pol,$nodes_used,$med,$mn,$mx,$emed,$sps,$last_ck,$EXPECT,$last_acc,$okstr,$REPEATS" >> "$CSV"
  done
  done
done

log "wrote $CSV  (logs in $LOGDIR/)"
