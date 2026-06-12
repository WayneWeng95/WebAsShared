#!/usr/bin/env bash
# run.sh — WebAsShared FINRA sweep (intra-node, shared-memory page-chain).
#
# Sweeps the input size (number of trades) over the native 8-rule FINRA DAG and
# writes results.csv in the shared shape:
#
#     size_trades,topo,compute_ms,throughput_trades_s,peak_mem_mb,reps,total_violations
#
# - compute_ms      : "[DAG][timing] TOTAL compute" (input staging EXCLUDED).
# - throughput      : size_trades / (compute_ms/1000).
# - peak_mem_mb     : peak(Σ RSS of host tree + SHM file size), sampled @50 ms.
# - total_violations: the correctness gate — MUST match across every system for a
#                     given trades file (deterministic generator).
#
# AOT vs JIT: pass WC_WASM=<guest.cwasm> for the AOT line (gen_dag.py honors it);
# WC_CSV overrides the output path (e.g. results_aot.csv).
#
# Usage:
#   ./run.sh                          # sizes 10000 100000 1000000, 5 reps
#   ./run.sh "10000 100000 1000000" 5
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

SIZES="${1:-10000 100000 1000000}"
REPEATS="${2:-5}"
TOPO="intra-shm"

NODE_AGENT="$ROOT/node-agent"
HOST_BIN="$ROOT/Executor/target/release/host"
SHM="/dev/shm/finra_sweep"
OUT="$HERE/.finra_out.txt"
CSV="${WC_CSV:-$HERE/results.csv}"

log() { printf '\033[1;36m[finra]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[finra] %s\033[0m\n' "$*" >&2; exit 1; }
[ -x "$NODE_AGENT" ] || die "node-agent not built (./build.sh)"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

sample_peak() {
  local outfile="$1"; local peak=0; echo 0 > "$outfile"
  while :; do
    local rss_kb shm_kb tot
    rss_kb=$(for p in $(pgrep -f "target/release/host" 2>/dev/null); do
               awk '/^VmRSS:/{print $2}' "/proc/$p/status" 2>/dev/null; done \
             | awk '{s+=$1} END{print s+0}')
    shm_kb=0; [ -f "$SHM" ] && shm_kb=$(( $(stat -c%s "$SHM" 2>/dev/null || echo 0) / 1024 ))
    tot=$(( rss_kb + shm_kb )); [ "$tot" -gt "$peak" ] && { peak=$tot; echo "$peak" > "$outfile"; }
    sleep 0.05
  done
}

echo "size_trades,topo,compute_ms,throughput_trades_s,peak_mem_mb,reps,total_violations" > "$CSV"
log "root=$ROOT  sizes=[$SIZES]  reps=$REPEATS  guest=${WC_WASM:-guest.wasm (JIT)}"
printf "%8s %12s %14s %9s %12s\n" "trades" "compute_ms" "trades/s" "peak_MB" "violations"

for N in $SIZES; do
  corpus="$ROOT/TestData/finra/trades_${N}.csv"
  [ -f "$corpus" ] || { log "SKIP (missing): $corpus  (run gen_trades.py $N)"; continue; }
  dag="$HERE/.dag_${N}.json"
  python3 "$HERE/gen_dag.py" "$corpus" "$OUT" "$SHM" > "$dag"

  ms_list=(); peak_list=(); vio=""
  for _ in $(seq 1 "$REPEATS"); do
    rm -f "$SHM" "$OUT" 2>/dev/null || true
    peakfile="$(mktemp)"; sample_peak "$peakfile" & sampler=$!
    res=$(cd "$ROOT" && "$NODE_AGENT" run "$dag" 2>&1); rc=$?
    kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null || true
    [ $rc -eq 0 ] || { echo "$res" | tail -6; rm -f "$peakfile"; die "run failed at N=$N"; }
    ms=$(echo "$res" | grep -oE "TOTAL compute: [0-9]+\.[0-9]+" | grep -oE "[0-9.]+" | head -1)
    ms_list+=("$ms")
    peak_list+=("$(cat "$peakfile" 2>/dev/null || echo 0)"); rm -f "$peakfile"
    vio=$(grep -oE "total_violations=[0-9]+" "$OUT" 2>/dev/null | grep -oE "[0-9]+" | head -1)
  done

  med_ms=$(median "${ms_list[@]}")
  peak_mb=$(awk "BEGIN{printf \"%.1f\", $(median "${peak_list[@]}")/1024}")
  tps=$(awk "BEGIN{printf \"%.0f\", $N/($med_ms/1000)}")
  echo "$N,$TOPO,$med_ms,$tps,$peak_mb,$REPEATS,${vio:-NA}" >> "$CSV"
  printf "%8s %12s %14s %9s %12s\n" "$N" "$med_ms" "$tps" "$peak_mb" "${vio:-NA}"
done

rm -f "$HERE"/.dag_*.json "$OUT" "$SHM" 2>/dev/null || true
log "wrote $CSV"
