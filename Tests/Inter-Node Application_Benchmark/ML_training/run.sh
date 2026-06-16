#!/usr/bin/env bash
# run.sh — WebAsShared synchronous-SGD sweep (intra-node, shared-memory page-chain).
#
# Sweeps (dataset size × gradient-worker fan-out W) over the native SGD DAG
# (E epochs unrolled) and writes results.csv in the shared column shape:
#
#   size_mb,workers,topo,compute_ms,throughput_mb_s,samples_per_s,peak_mem_mb,reps,checksum,accuracy
#
# - compute_ms      : "[DAG][timing] TOTAL compute" (staging EXCLUDED, per TEST_PLAN).
# - throughput_mb_s : size_mb / (compute_ms/1000).
# - samples_per_s   : n_samples * epochs / (compute_ms/1000)   (sample-gradients/s).
# - peak_mem_mb     : peak (Σ RSS of host tree + SHM file size), sampled at 50 ms —
#                     the billable-memory high-water (private RSS + SHM, SHM once).
# - reps            : repeats per cell; compute_ms/throughput are the MEDIAN.
# - checksum        : Σ of all model weights after E epochs — the CORRECTNESS GATE.
#                     The gradient is an integer SUM, divided once centrally, so the
#                     checksum is FAN-OUT-INVARIANT: it MUST be identical at every W
#                     for a given size (the harness flags any mismatch).
# - accuracy        : final train accuracy (deterministic, reported for sanity).
#
# Usage:
#   ./run.sh                                       # default: 100k 300k 600k, W=1..16, 3 reps
#   ./run.sh "100000"                              # one size (sample count)
#   ./run.sh "100000 300000 600000" "1 2 4 8 16" 3 # sizes, fan-outs, reps
# Env: ML_WASM → guest module (.cwasm for AOT); ML_CSV → output path;
#      ML_EPOCHS → epochs (default 10); ML_SEED → dataset seed (default 1234).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"          # → WebAsShared/

SIZES="${1:-100000 300000 600000}"
FANOUTS="${2:-1 2 4 8 16}"
REPEATS="${3:-3}"
EPOCHS="${ML_EPOCHS:-10}"
SEED="${ML_SEED:-1234}"
TOPO="intra-shm"

NODE_AGENT="$ROOT/node-agent"
[ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
HOST_BIN="$ROOT/Executor/target/release/host"
DATADIR="$ROOT/TestData/ml"
SHM="/dev/shm/ml_sgd_appbench"
CSV="${ML_CSV:-$HERE/results.csv}"   # override for variant runs (e.g. results_aot.csv)
OUT="$HERE/.ml_out.txt"

log() { printf '\033[1;36m[sgd]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[sgd] %s\033[0m\n' "$*" >&2; exit 1; }

[ -x "$NODE_AGENT" ] || die "node-agent not built (run ./build.sh)"
[ -x "$HOST_BIN" ]   || die "host executor not built: $HOST_BIN (run ./build.sh)"
mkdir -p "$DATADIR"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# Background peak-memory sampler: max(Σ RSS of host tree + SHM file size) in KB.
sample_peak() {
  local outfile="$1"; local peak=0
  echo 0 > "$outfile"
  while :; do
    local rss_kb shm_kb tot
    rss_kb=$(for p in $(pgrep -f "target/release/host" 2>/dev/null); do
               awk '/^VmRSS:/{print $2}' "/proc/$p/status" 2>/dev/null; done \
             | awk '{s+=$1} END{print s+0}')
    shm_kb=0
    [ -f "$SHM" ] && shm_kb=$(( $(stat -c%s "$SHM" 2>/dev/null || echo 0) / 1024 ))
    tot=$(( rss_kb + shm_kb ))
    [ "$tot" -gt "$peak" ] && { peak=$tot; echo "$peak" > "$outfile"; }
    sleep 0.05
  done
}

echo "size_mb,workers,topo,compute_ms,throughput_mb_s,samples_per_s,peak_mem_mb,reps,checksum,accuracy" > "$CSV"
log "root=$ROOT  sizes=[$SIZES]  fanouts=[$FANOUTS]  reps=$REPEATS  epochs=$EPOCHS  cores=$(nproc)"
printf "%9s %4s %12s %9s %12s %9s %12s %8s\n" "size_MB" "W" "compute_ms" "MB/s" "samp/s" "peak_MB" "checksum" "acc%"

for NS in $SIZES; do
  DATA="$DATADIR/sgd_${NS}.csv"
  if [ ! -f "$DATA" ]; then
    log "generating $NS-sample dataset"
    python3 "$HERE/gen_data.py" "$NS" "$DATA" "$SEED" >/dev/null || die "gen_data failed NS=$NS"
  fi
  bytes=$(wc -c < "$DATA")
  size_mb=$(awk "BEGIN{printf \"%.1f\", $bytes/1048576}")

  base_ck=""
  for W in $FANOUTS; do
    dag="$HERE/.dag_n${NS}_w${W}.json"
    python3 "$HERE/gen_dag.py" "$W" "$EPOCHS" "$DATA" "$OUT" "$SHM" > "$dag"

    ms_list=(); peak_list=(); ck=""; acc=""; crashed=0; reason=""
    for _ in $(seq 1 "$REPEATS"); do
      rm -f "$SHM" "$OUT" 2>/dev/null || true
      peakfile="$(mktemp)"
      sample_peak "$peakfile" & sampler=$!
      res=$(cd "$ROOT" && "$NODE_AGENT" run "$dag" 2>&1); rc=$?
      kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null || true
      if [ $rc -ne 0 ]; then
        crashed=1
        reason=$(echo "$res" | grep -oiE "unreachable|capacity|exhausted|OOM|out of memory|cannot allocate" | head -1)
        rm -f "$peakfile"; break
      fi
      ms=$(echo "$res" | grep -oE "TOTAL compute: [0-9]+\.[0-9]+" | grep -oE "[0-9]+\.[0-9]+" | head -1)
      [ -n "$ms" ] || { echo "$res" | tail -8; rm -f "$peakfile"; die "no TOTAL compute at NS=$NS W=$W"; }
      ms_list+=("$ms")
      peak_kb=$(cat "$peakfile" 2>/dev/null || echo 0); rm -f "$peakfile"
      peak_list+=("$peak_kb")
      ck=$(grep -oE "weight_checksum=[0-9-]+" "$OUT" 2>/dev/null | grep -oE "[0-9-]+" | head -1)
      acc=$(grep -oE "accuracy=[0-9.]+" "$OUT" 2>/dev/null | grep -oE "[0-9.]+" | head -1)
    done

    if [ "$crashed" = 1 ]; then
      echo "$size_mb,$W,$TOPO,CRASH,,,,${REPEATS},${reason:-fail}," >> "$CSV"
      printf "%9s %4s %12s\n" "$size_mb" "$W" "CRASH(${reason:-fail})"
      continue
    fi

    med_ms=$(median "${ms_list[@]}")
    med_peak_kb=$(median "${peak_list[@]}")
    peak_mb=$(awk "BEGIN{printf \"%.1f\", $med_peak_kb/1024}")
    mbps=$(awk "BEGIN{printf \"%.1f\", $size_mb/($med_ms/1000)}")
    sps=$(awk "BEGIN{printf \"%.0f\", $NS*$EPOCHS/($med_ms/1000)}")
    [ -z "$base_ck" ] && base_ck="$ck"
    flag=""; [ "$ck" != "$base_ck" ] && flag="  <-- CHECKSUM MISMATCH (want $base_ck)"
    echo "$size_mb,$W,$TOPO,$med_ms,$mbps,$sps,$peak_mb,$REPEATS,${ck:-NA},${acc:-NA}" >> "$CSV"
    printf "%9s %4s %12s %9s %12s %9s %12s %8s%s\n" \
      "$size_mb" "$W" "$med_ms" "$mbps" "$sps" "$peak_mb" "${ck:-NA}" "${acc:-NA}" "$flag"
  done
done

rm -f "$HERE"/.dag_n*.json "$OUT" "$SHM" 2>/dev/null || true
log "wrote $CSV"
