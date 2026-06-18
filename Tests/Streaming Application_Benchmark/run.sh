#!/usr/bin/env bash
# run.sh — WebAsShared streaming-application sweep (RTSFaaS MediaReview /
# SocialNetwork re-implemented on our stream model; intra-node, SHM page-chain).
#
# Sweeps the event count for one workload and writes results.csv:
#
#   workload,events,users,topo,compute_ms,throughput_events_s,peak_mem_mb,reps,total_events,login_ok
#
# - compute_ms      : "[DAG][timing] TOTAL compute" (input staging EXCLUDED).
# - throughput      : events / (compute_ms/1000).
# - peak_mem_mb     : peak(Σ RSS of host tree + SHM file size), sampled @50 ms.
# - total_events/login_ok : the correctness gate — deterministic (gen_events.py
#                     fixed seed); login_ok MUST equal #login events every run.
#
# Usage:
#   ./run.sh mediareview                     # sizes 100000 1000000, 5 reps
#   ./run.sh socialnetwork "100000 1000000" 3
#   WC_WASM=<guest.cwasm> ./run.sh ...       # AOT line (gen_dag.py honors it)
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

WORKLOAD="${1:-mediareview}"
SIZES="${2:-100000 1000000}"
REPEATS="${3:-5}"
USERS="${USERS:-10000}"
PARTS="${PARTS:-4}"
TOPO="intra-shm"

NODE_AGENT="$ROOT/node-agent"
SHM="/dev/shm/stream_app_${WORKLOAD}"
OUT="$HERE/.${WORKLOAD}_out.txt"
CSV="${WC_CSV:-$HERE/results_${WORKLOAD}.csv}"

log() { printf '\033[1;36m[stream-app]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[stream-app] %s\033[0m\n' "$*" >&2; exit 1; }
[ -x "$NODE_AGENT" ] || die "node-agent not built (./build.sh)"
case "$WORKLOAD" in mediareview|socialnetwork) ;; *) die "unknown workload: $WORKLOAD" ;; esac

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

sample_peak() {
  local outfile="$1"; local peak=0; echo 0 > "$outfile"
  while :; do
    local rss_kb shm_kb tot
    rss_kb=$(for pp in $(pgrep -f "target/release/host" 2>/dev/null); do
               awk '/^VmRSS:/{print $2}' "/proc/$pp/status" 2>/dev/null; done \
             | awk '{s+=$1} END{print s+0}')
    shm_kb=0; [ -f "$SHM" ] && shm_kb=$(( $(stat -c%s "$SHM" 2>/dev/null || echo 0) / 1024 ))
    tot=$(( rss_kb + shm_kb )); [ "$tot" -gt "$peak" ] && { peak=$tot; echo "$peak" > "$outfile"; }
    sleep 0.05
  done
}

echo "workload,events,users,topo,compute_ms,throughput_events_s,peak_mem_mb,reps,total_events,login_ok" > "$CSV"
log "root=$ROOT  workload=$WORKLOAD  sizes=[$SIZES]  users=$USERS  parts=$PARTS  reps=$REPEATS"
printf "%9s %12s %14s %9s %12s %9s\n" "events" "compute_ms" "events/s" "peak_MB" "total_ev" "login_ok"

for N in $SIZES; do
  events="$HERE/.events_${WORKLOAD}_${N}.csv"
  python3 "$HERE/gen_events.py" "$WORKLOAD" --events "$N" --users "$USERS" --skew "${SKEW:-0.0}" > "$events"
  dag="$HERE/.dag_${WORKLOAD}_${N}.json"
  python3 "$HERE/gen_dag.py" "$WORKLOAD" "$events" "$OUT" "$SHM" --partitions "$PARTS" --users "$USERS" > "$dag"

  ms_list=(); peak_list=(); tot=""; ok=""
  for _ in $(seq 1 "$REPEATS"); do
    rm -f "$SHM" "$OUT" 2>/dev/null || true
    peakfile="$(mktemp)"; sample_peak "$peakfile" & sampler=$!
    res=$(cd "$ROOT" && "$NODE_AGENT" run "$dag" 2>&1); rc=$?
    kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null || true
    [ $rc -eq 0 ] || { echo "$res" | tail -8; rm -f "$peakfile"; die "run failed at N=$N"; }
    ms=$(echo "$res" | grep -oE "TOTAL compute: [0-9]+\.[0-9]+" | grep -oE "[0-9.]+" | head -1)
    ms_list+=("$ms")
    peak_list+=("$(cat "$peakfile" 2>/dev/null || echo 0)"); rm -f "$peakfile"
    tot=$(grep -oE "total_events=[0-9]+" "$OUT" 2>/dev/null | grep -oE "[0-9]+" | head -1)
    ok=$(grep -oE "login_ok=[0-9]+" "$OUT" 2>/dev/null | grep -oE "[0-9]+" | head -1)
  done

  med_ms=$(median "${ms_list[@]}")
  peak_mb=$(awk "BEGIN{printf \"%.1f\", $(median "${peak_list[@]}")/1024}")
  tps=$(awk "BEGIN{printf \"%.0f\", $N/($med_ms/1000)}")
  echo "$WORKLOAD,$N,$USERS,$TOPO,$med_ms,$tps,$peak_mb,$REPEATS,${tot:-NA},${ok:-NA}" >> "$CSV"
  printf "%9s %12s %14s %9s %12s %9s\n" "$N" "$med_ms" "$tps" "$peak_mb" "${tot:-NA}" "${ok:-NA}"
done

rm -f "$HERE"/.dag_*.json "$HERE"/.events_*.csv "$OUT" "$SHM" 2>/dev/null || true
log "wrote $CSV"
