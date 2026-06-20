#!/usr/bin/env bash
# verify.sh — correctness check for the multi-node workloads on the live cluster.
#
# For each workload it runs the SAME job two ways through node-agent submit:
#   • ground truth  — gen_variants --nodes 1  (everything on the coordinator)
#   • distributed   — gen_variants --nodes N  (balanced, fanned across the cluster)
# and asserts the FAN-OUT-INVARIANT correctness gate is identical. The gate is a
# plain integer that cannot change with placement/sharding, so GT == distributed
# proves the cross-node path (RDMA gather/shuffle) neither dropped nor duplicated
# any state. Defaults to the three newly-added workloads; pass names to override.
#
# Usage:
#   ./verify.sh                         # ml_inference matrix terasort, NODES=4
#   NODES=4 ./verify.sh ml_inference    # one workload
#   ./verify.sh word_count finra ml_training ml_inference matrix terasort   # all six
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
cd "$ROOT"
gv() { python3 "$HERE/gen_variants.py" "$@"; }
PART="$ROOT/Partitioner/target/release/partition"
NA="$ROOT/node-agent"; [ -x "$NA" ] || NA="$ROOT/NodeAgent/target/release/node-agent"
CFG="$ROOT/NodeAgent/agent.toml"
NODES="${NODES:-4}"
TMP="$HERE/.verify"; mkdir -p "$TMP" "$ROOT/TestOutput"
WORKLOADS=("$@"); [ ${#WORKLOADS[@]} -gt 0 ] || WORKLOADS=(ml_inference matrix terasort)

# Per-workload gen_variants args (fanout/input/etc.). terasort uses N==nodes.
gen_args() { case "$1" in
  word_count)   echo "--input TestData/corpus_xlarge.txt --fanout 8" ;;
  finra)        echo "--fanout 32 --pack-cap 16" ;;
  ml_training)  echo "--fanout 8" ;;
  ml_inference) echo "--fanout 8" ;;
  matrix)       echo "--fanout 4 --matrix-n 512" ;;
  terasort)     echo "--input TestData/terasort/records_32mb.txt --fanout __NODES__" ;;
esac; }

# Extract the fan-out-invariant gate from the result file(s). Echoes a single string.
gate() { local wl="$1"; case "$wl" in
  ml_inference) grep -oE "prediction_checksum=[0-9]+" TestOutput/ml_inference_ap_result.txt 2>/dev/null | head -1 ;;
  matrix)       grep -oE "checksum=-?[0-9]+" TestOutput/matrix_ap_result.txt 2>/dev/null | head -1 ;;
  terasort)     local f=TestOutput/terasort_ap_result.txt
                [ -s "$f" ] || return
                local r k rg so
                r=$(grep -oE "^records=[0-9]+" "$f" | grep -oE "[0-9]+")
                k=$(grep -oE "^keysum=[0-9]+" "$f" | grep -oE "[0-9]+")
                rg=$(grep -oE "^ranges=[0-9]+" "$f" | grep -oE "[0-9]+")
                so=$(grep -oE "^sorted=[0-9]+" "$f" | grep -oE "[0-9]+")
                echo "records=$r keysum=$k ranges=$rg sorted=$so" ;;
  word_count)   grep -oE "total_words=[0-9]+|unique_words=[0-9]+" TestOutput/wc_auto_placement_result.txt 2>/dev/null | paste -sd' ' ;;
  finra)        grep -oE "violations=[0-9]+" TestOutput/finra_ap_result.txt 2>/dev/null | head -1 ;;
  ml_training)  grep -oE "weight_checksum=-?[0-9]+" TestOutput/ml_training_ap_result.txt 2>/dev/null | head -1 ;;
esac; }

# Clear the result file(s) so a stale read can't masquerade as success.
clear_out() { case "$1" in
  ml_inference) rm -f TestOutput/ml_inference_ap_result.txt ;;
  matrix)       rm -f TestOutput/matrix_ap_result.txt ;;
  terasort)     rm -f TestOutput/terasort_ap_result.txt TestOutput/terasort_ap_result.part* ;;
  word_count)   rm -f TestOutput/wc_auto_placement_result.txt ;;
  finra)        rm -f TestOutput/finra_ap_result.txt ;;
  ml_training)  rm -f TestOutput/ml_training_ap_result.txt ;;
esac; }

# run <wl> <nodes> <tag> → echo "<wall_ms> | <gate>"  (wall=-1 on failure)
run() {
  local wl="$1" n="$2" tag="$3" args; args="$(gen_args "$wl")"; args="${args//__NODES__/$n}"
  local sym="$TMP/${wl}_${tag}.sym.json" cdag="$TMP/${wl}_${tag}.cdag.json" log="$TMP/${wl}_${tag}.log"
  if ! gv "$wl" --nodes "$n" $args --out "$sym" >"$TMP/gen.err" 2>&1; then echo "-1 | GEN_FAIL"; cat "$TMP/gen.err" >&2; return; fi
  if ! "$PART" "$sym" --nodes "$n" >"$cdag" 2>"$TMP/part.err"; then echo "-1 | PART_FAIL"; cat "$TMP/part.err" >&2; return; fi
  clear_out "$wl"
  timeout 300 "$NA" submit --config "$CFG" --dag "$cdag" >"$log" 2>&1
  local wall; wall=$(grep -oiE "total wall time:[[:space:]]*[0-9]+ms" "$log" | grep -oE "[0-9]+" | head -1)
  grep -qiE "Success:[[:space:]]*true" "$log" || wall="-1"
  echo "${wall:--1} | $(gate "$wl")"
}

echo "Cluster verify  (NODES=$NODES, ground-truth vs distributed)"
printf "%-13s %12s %12s  %-7s  %s\n" "workload" "gt_ms" "dist_ms" "gate_ok" "gate (gt | dist)"
fails=0
for wl in "${WORKLOADS[@]}"; do
  IFS='|' read -r gt_ms gt_gate < <(run "$wl" 1 gt);          gt_gate="${gt_gate# }"
  IFS='|' read -r d_ms  d_gate  < <(run "$wl" "$NODES" dist); d_gate="${d_gate# }"
  # PASS requires: both submits succeeded (wall ≥ 0), the gate is non-empty, and
  # GT == distributed. (Guards against an empty==empty false match.)
  if [ "${gt_ms# }" != "-1" ] && [ "${d_ms# }" != "-1" ] && [ -n "$gt_gate" ] && [ "$gt_gate" = "$d_gate" ]; then ok=PASS; else ok=FAIL; fi
  [ "$ok" = PASS ] || fails=$((fails+1))
  printf "%-13s %12s %12s  %-7s  %s  |  %s\n" "$wl" "${gt_ms# }" "${d_ms# }" "$ok" "$gt_gate" "$d_gate"
done
echo ""
[ "$fails" -eq 0 ] && echo "ALL PASS ✓" || { echo "$fails workload(s) FAILED ✗"; exit 1; }
