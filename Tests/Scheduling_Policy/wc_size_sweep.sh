#!/usr/bin/env bash
# wc_size_sweep.sh — word_count across (input size × placement policy) on the cluster.
#
# Focused sub-experiment: word_count is the policy-INSENSITIVE control (its heavy
# stages are placement:"all", so all four policies place work identically). Sweeping
# input size shows (a) end-to-end scaling with corpus size, and (b) that the policy
# does NOT move latency at any size — the control holding across the size axis.
#
# (finra/ml_training are excluded here: their distributing policies deadlock at
# runtime in the executor's cross-node gather — to be revisited separately.)
#
# Pre-partitions OFFLINE (embedded policy authoritative; see prepartition.sh) for
# each (policy, size), then submits the finished ClusterDag and times it.
#
# PREREQUISITES: cluster up (coordinator + workers), corpora present on node-0.
#
# Usage:
#   ./wc_size_sweep.sh [REPEATS] [CONFIG]
#   SIZES / POLICIES env override the axes. SETTLE_S between runs (default 3).
#
#   # default: 3 sizes × 4 policies × 3 reps
#   ./wc_size_sweep.sh
#   SIZES="corpus_large.txt corpus_xlarge.txt" ./wc_size_sweep.sh 5
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

REPEATS="${1:-3}"
CONFIG="${2:-$ROOT/NodeAgent/agent.toml}"
NODES="${NODES:-4}"
# Corpora in TestData/, smallest→largest. Sizes are auto-measured per file.
SIZES="${SIZES:-corpus_large.txt corpus_xlarge.txt corpus.txt}"
POLICIES="${POLICIES:-pack balanced}"
SETTLE_S="${SETTLE_S:-3}"
# Map-stage parallelism: TOTAL map workers across the cluster, swept as a group.
# On 4×16C/32T with PACK_CAP=16: pack consolidates first-fit (fanout 32 → 2 nodes
# [16,16,0,0]; 48 → 3 nodes [16,16,16,0]) while balanced spreads evenly across 4.
FANOUTS="${FANOUTS:-32 48}"
# Per-node worker cap that drives pack's first-fit consolidation (= cores/node).
PACK_CAP="${PACK_CAP:-16}"

NODE_AGENT="$ROOT/node-agent"
[ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
PART="$ROOT/Partitioner/target/release/partition"
CDIR="$HERE/cluster_dags"
LOGDIR="$HERE/logs_wc_size"
CSV="$HERE/analysis/results_wc_size.csv"

log() { printf '\033[1;36m[wc-size]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[wc-size] %s\033[0m\n' "$*" >&2; exit 1; }
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

[ -x "$NODE_AGENT" ] || die "node-agent not built: $NODE_AGENT"
[ -x "$PART" ]       || die "partition binary not built: $PART"
[ -f "$CONFIG" ]     || die "config not found: $CONFIG"
mkdir -p "$CDIR" "$LOGDIR"

# makespan_ms = end-to-end latency (coordinator total wall time).
# total_exec_ms = Σ per-node busy time over WORKING nodes (resource cost, idle excluded).
echo "size_mb,corpus,fanout,policy,nodes_used,makespan_ms_median,makespan_ms_min,makespan_ms_max,maxcompute_ms_median,total_exec_ms_median,throughput_mb_s,success,reps" > "$CSV"
log "nodes=$NODES reps=$REPEATS sizes=[$SIZES] fanouts=[$FANOUTS] policies=[$POLICIES] pack_cap=$PACK_CAP settle=${SETTLE_S}s"
printf "%8s %4s %-9s %6s %11s %11s %10s %6s\n" "size_MB" "fan" "policy" "nodes" "makespan" "total_exec" "MB/s" "ok"

for corpus in $SIZES; do
  CORPUS_FILE="$ROOT/TestData/$corpus"
  if [ ! -f "$CORPUS_FILE" ]; then log "SKIP (missing): TestData/$corpus"; continue; fi
  bytes=$(wc -c < "$CORPUS_FILE")
  size_mb=$(awk "BEGIN{printf \"%.0f\", $bytes/1048576}")

  for fo in $FANOUTS; do
  for pol in $POLICIES; do
    tag="word_count__${pol}__${corpus%.txt}__f${fo}"
    sym="$CDIR/$tag.symbolic.json"
    cdag="$CDIR/$tag.json"
    python3 "$HERE/gen_variants.py" word_count "$pol" --nodes "$NODES" \
      --input "TestData/$corpus" --fanout "$fo" --pack-cap "$PACK_CAP" --out "$sym" || die "gen failed $tag"
    "$PART" "$sym" --nodes "$NODES" > "$cdag" 2>"$cdag.err" || { cat "$cdag.err"; die "partition failed $tag"; }
    rm -f "$cdag.err"
    # Working node IDs (ran ≥1 map worker). Idle nodes are excluded from
    # total_exec so pack's spare nodes (~0.5s spin-up) don't pollute the cost.
    work_ids=$(python3 - "$cdag" <<'PY'
import json,sys
nd=json.load(open(sys.argv[1]))["node_dags"]
work=[k for k in sorted(nd,key=int) if any(isinstance(n.get('kind'),dict) and n['kind'].get('WasmVoid',{}).get('func')=='wc_map' for n in nd[k])]
print(' '.join(work))
PY
)
    nodes_used=$(echo $work_ids | wc -w)

    ms_list=(); comp_list=(); exec_list=(); ok_all=1
    for r in $(seq 1 "$REPEATS"); do
      runlog="$LOGDIR/$tag.rep${r}.log"
      t0=$(date +%s%N)
      ( cd "$ROOT" && "$NODE_AGENT" submit --config "$CONFIG" --dag "$cdag" ) >"$runlog" 2>&1
      rc=$?; t1=$(date +%s%N); ext_ms=$(( (t1-t0)/1000000 ))
      ok=0; grep -qiE "Success:[[:space:]]*true" "$runlog" && ok=1; [ $rc -ne 0 ] && ok=0
      [ "$ok" = 1 ] || ok_all=0
      # makespan = coordinator-reported total wall time (≈ slowest node).
      wall=$(grep -oiE "total wall time:[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | head -1)
      [ -n "$wall" ] || wall="$ext_ms"
      comp=$(grep -oiE "\):[[:space:]]*[0-9]+ms" "$runlog" | grep -oE "[0-9]+" | sort -n | tail -1)
      [ -n "$comp" ] || comp=0
      # total_exec = Σ per-node busy time over WORKING nodes only (resource cost).
      texec=$(WORK="$work_ids" python3 - "$runlog" <<'PY'
import os,re,sys
work=set(os.environ.get("WORK","").split())
text=open(sys.argv[1]).read()
times={n:int(ms) for n,ms in re.findall(r"node\s+(\d+)\s+\([^)]*\):\s+(\d+)ms",text,re.I)}
print(sum(t for n,t in times.items() if n in work) if times else 0)
PY
)
      ms_list+=("$wall"); comp_list+=("$comp"); exec_list+=("$texec")
      sleep "$SETTLE_S"
    done

    med=$(median "${ms_list[@]}")
    mn=$(printf '%s\n' "${ms_list[@]}" | sort -n | head -1)
    mx=$(printf '%s\n' "${ms_list[@]}" | sort -n | tail -1)
    cmed=$(median "${comp_list[@]}")
    emed=$(median "${exec_list[@]}")
    mbps=$(awk "BEGIN{ if ($med>0) printf \"%.1f\", $size_mb/($med/1000); else print \"NA\" }")
    okstr=$([ "$ok_all" = 1 ] && echo true || echo false)

    printf "%8s %4s %-9s %6s %11s %11s %10s %6s\n" "$size_mb" "$fo" "$pol" "$nodes_used" "$med" "$emed" "$mbps" "$okstr"
    echo "$size_mb,$corpus,$fo,$pol,$nodes_used,$med,$mn,$mx,$cmed,$emed,$mbps,$okstr,$REPEATS" >> "$CSV"
  done
  done
done

log "wrote $CSV  (logs in $LOGDIR/)"
