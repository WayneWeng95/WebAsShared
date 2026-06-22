#!/usr/bin/env bash
# sweep_rtsfaas.sh — RTSFaaS cluster throughput sweep across request sizes.
# RTSFaaS's load knob is `number` in Env/System.env (sendMessagePerFrontend =
# number*threadNum*workerNum/frontendNum). We sweep it upward, take the client's
# end-to-end "Throughput: records/sec" per run, and report the peak. Above some
# `number` RTSFaaS stops completing (no result within --deadline) — we stop there.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKLOAD="${1:-SocialNetwork}"
NUMBERS="${NUMBERS:-100 250 500 750 1000 1500 2000}"
DEADLINE="${DEADLINE:-200}"
CSV="$HERE/results_rtsfaas_sweep_${WORKLOAD}.csv"
SYS="$HERE/Env/System.env"
orig=$(grep -oE '^number=[0-9]+' "$SYS" | cut -d= -f2)
echo "workload,number,throughput_req_s,avg_ms,p99_ms,status" > "$CSV"

best_thr=0; best_line=""
for N in $NUMBERS; do
  sed -i "s/^number=.*/number=$N/" "$SYS"
  echo ">>> $WORKLOAD number=$N (deadline ${DEADLINE}s)"
  out=$("$HERE/rt-coordinator.py" "$WORKLOAD" --reps 1 --deadline "$DEADLINE" --csv /tmp/rt_sweep_throwaway.csv 2>&1)
  line=$(echo "$out" | grep -oE "'throughput_req_s': [^,]+" | head -1)
  thr=$(echo "$line" | grep -oE "[0-9]+\.[0-9]+|None" | head -1)
  avg=$(echo "$out" | grep -oE "'avg_ms': [^,]+" | grep -oE "[0-9]+\.[0-9]+|None" | head -1)
  p99=$(echo "$out" | grep -oE "'p99_ms': [^,]+" | grep -oE "[0-9]+\.[0-9]+|None" | head -1)
  if [ "$thr" = "None" ] || [ -z "$thr" ]; then
    echo "$WORKLOAD,$N,NA,NA,NA,DID_NOT_COMPLETE" | tee -a "$CSV"
    echo "    -> did not complete within ${DEADLINE}s; stopping the sweep (higher load won't help)."
    break
  fi
  echo "$WORKLOAD,$N,$thr,$avg,$p99,ok" | tee -a "$CSV"
  ti=${thr%.*}
  if [ "$ti" -gt "$best_thr" ]; then best_thr=$ti; best_line="number=$N  ${thr} req/s  (avg ${avg}ms, p99 ${p99}ms)"; fi
done
sed -i "s/^number=.*/number=${orig:-500}/" "$SYS"   # restore
echo
echo "==================== RTSFaaS PEAK THROUGHPUT ===================="
echo "  $WORKLOAD ($(grep -oE '^workerNum=[0-9]+' "$HERE/Env/Cluster.env" | cut -d= -f2) workers):  $best_line"
echo "  full table -> $CSV"
