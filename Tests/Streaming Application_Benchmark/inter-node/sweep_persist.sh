#!/usr/bin/env bash
# sweep_persist.sh — WasMem cluster streaming throughput sweep across request sizes
# in three FT modes (none / async Persist / sync Persist). Computes req/s =
# events / total-wall-ms and records every run + reports the peak.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKLOAD="${1:-socialnetwork}"
NODES="${NODES:-4}"
SIZES="${SIZES:-200000 500000 1000000 2000000 4000000 8000000}"
MODES="${MODES:-none async sync}"
CSV="$HERE/results_persist_sweep_${WORKLOAD}.csv"
echo "workload,persist,nodes,events,wall_ms,throughput_req_s,success" > "$CSV"

best_thr=0; best_line=""
for mode in $MODES; do
  for ev in $SIZES; do
    out=$(PERSIST="$mode" NODES="$NODES" "$HERE/run-cluster.sh" "$WORKLOAD" "$ev" 2>&1)
    ok=$(echo "$out" | grep -oE "Success: (true|false)" | awk '{print $2}')
    ms=$(echo "$out" | grep -oE "total wall time: [0-9]+ms" | grep -oE "[0-9]+")
    if [ "$ok" = "true" ] && [ -n "$ms" ]; then
      thr=$(python3 -c "print(round($ev/($ms/1000)))")
      echo "$WORKLOAD,$mode,$NODES,$ev,$ms,$thr,true" | tee -a "$CSV"
      if [ "$thr" -gt "$best_thr" ]; then best_thr=$thr; best_line="$mode  events=$ev  wall=${ms}ms  ${thr} req/s"; fi
    else
      echo "$WORKLOAD,$mode,$NODES,$ev,${ms:-NA},NA,${ok:-fail}" | tee -a "$CSV"
    fi
  done
done
echo
echo "==================== PEAK THROUGHPUT ===================="
echo "  $WORKLOAD ($NODES nodes):  $best_line"
echo "  full table -> $CSV"
