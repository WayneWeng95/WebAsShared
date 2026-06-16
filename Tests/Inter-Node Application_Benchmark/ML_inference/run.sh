#!/usr/bin/env bash
# run.sh — WebAsShared MNIST-inference sweep (intra-node, shared-memory page-chain).
#
# Sweeps (test-set size × predict-worker fan-out W) over the native inference DAG
# (single forward pass) and writes results.csv in the shared column shape:
#
#   size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,reps,checksum,accuracy
#
# - compute_ms    : "[DAG][timing] TOTAL compute" (staging EXCLUDED).
# - samples_per_s : n_samples / (compute_ms/1000)  (predictions/s).
# - checksum      : prediction_checksum = Σ predicted labels — the CORRECTNESS GATE.
#                   Each sample is classified independently, so it is FAN-OUT-INVARIANT:
#                   identical at every W for a given test set (harness flags mismatches).
# - accuracy      : test accuracy (deterministic).
#
# Usage:  ./run.sh ["100000 300000 600000"] ["1 2 4 8 16"] [reps]
# Env: MLI_WASM → guest module (.cwasm for AOT); MLI_CSV → output; MLI_SEED → test seed.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
GENDATA="$ROOT/Tests/Application_Benchmark/ML_training/gen_data.py"

SIZES="${1:-100000 300000 600000}"
FANOUTS="${2:-1 2 4 8 16}"
REPEATS="${3:-3}"
SEED="${MLI_SEED:-9999}"            # held-out test seed (≠ training seed)
TOPO="intra-shm"

NODE_AGENT="$ROOT/node-agent"; [ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
DATADIR="$ROOT/TestData/ml"; SHM="/dev/shm/mli_appbench"
MODEL="$DATADIR/infer_model.txt"
CSV="${MLI_CSV:-$HERE/results.csv}"; OUT="$HERE/.mli_out.txt"

log() { printf '\033[1;36m[infer]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[infer] %s\033[0m\n' "$*" >&2; exit 1; }
[ -x "$NODE_AGENT" ] || die "node-agent not built (run ./build.sh)"
mkdir -p "$DATADIR"
[ -f "$MODEL" ] || python3 "$HERE/gen_model.py" "$MODEL" >/dev/null

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }
sample_peak() {
  local outfile="$1"; local peak=0; echo 0 > "$outfile"
  while :; do
    local rss_kb shm_kb tot
    rss_kb=$(for p in $(pgrep -f "target/release/host" 2>/dev/null); do awk '/^VmRSS:/{print $2}' "/proc/$p/status" 2>/dev/null; done | awk '{s+=$1} END{print s+0}')
    shm_kb=0; [ -f "$SHM" ] && shm_kb=$(( $(stat -c%s "$SHM" 2>/dev/null || echo 0) / 1024 ))
    tot=$(( rss_kb + shm_kb )); [ "$tot" -gt "$peak" ] && { peak=$tot; echo "$peak" > "$outfile"; }
    sleep 0.05
  done
}

echo "size_mb,workers,topo,compute_ms,samples_per_s,peak_mem_mb,reps,checksum,accuracy" > "$CSV"
log "sizes=[$SIZES] fanouts=[$FANOUTS] reps=$REPEATS  cores=$(nproc)"
printf "%9s %4s %12s %12s %9s %12s %8s\n" "size_MB" "W" "compute_ms" "samp/s" "peak_MB" "checksum" "acc%"

for NS in $SIZES; do
  DATA="$DATADIR/test_${NS}.csv"
  [ -f "$DATA" ] || python3 "$GENDATA" "$NS" "$DATA" "$SEED" >/dev/null
  bytes=$(wc -c < "$DATA"); size_mb=$(awk "BEGIN{printf \"%.1f\", $bytes/1048576}")
  base_ck=""
  for W in $FANOUTS; do
    dag="$HERE/.dag_n${NS}_w${W}.json"
    python3 "$HERE/gen_dag.py" "$W" "$MODEL" "$DATA" "$OUT" "$SHM" > "$dag"  # gen_dag reads MLI_WASM from env
    ms_list=(); peak_list=(); ck=""; acc=""
    for _ in $(seq 1 "$REPEATS"); do
      rm -f "$SHM" "$OUT" 2>/dev/null || true
      pf="$(mktemp)"; sample_peak "$pf" & sp=$!
      res=$(cd "$ROOT" && "$NODE_AGENT" run "$dag" 2>&1); rc=$?
      kill "$sp" 2>/dev/null; wait "$sp" 2>/dev/null || true
      [ $rc -eq 0 ] || { echo "$res"|tail -6; rm -f "$pf"; die "run failed NS=$NS W=$W"; }
      ms=$(echo "$res" | grep -oE "TOTAL compute: [0-9.]+" | grep -oE "[0-9.]+" | head -1)
      ms_list+=("$ms"); peak_list+=("$(cat "$pf" 2>/dev/null||echo 0)"); rm -f "$pf"
      ck=$(grep -oE "prediction_checksum=[0-9-]+" "$OUT"|grep -oE "[0-9-]+"|head -1)
      acc=$(grep -oE "accuracy=[0-9.]+" "$OUT"|grep -oE "[0-9.]+"|head -1)
    done
    med_ms=$(median "${ms_list[@]}"); med_pk=$(median "${peak_list[@]}")
    peak_mb=$(awk "BEGIN{printf \"%.1f\", $med_pk/1024}")
    sps=$(awk "BEGIN{printf \"%.0f\", $NS/($med_ms/1000)}")
    [ -z "$base_ck" ] && base_ck="$ck"
    flag=""; [ "$ck" != "$base_ck" ] && flag="  <-- CHECKSUM MISMATCH (want $base_ck)"
    echo "$size_mb,$W,$TOPO,$med_ms,$sps,$peak_mb,$REPEATS,${ck:-NA},${acc:-NA}" >> "$CSV"
    printf "%9s %4s %12s %12s %9s %12s %8s%s\n" "$size_mb" "$W" "$med_ms" "$sps" "$peak_mb" "${ck:-NA}" "${acc:-NA}" "$flag"
  done
done
rm -f "$HERE"/.dag_n*.json "$OUT" "$SHM" 2>/dev/null || true
log "wrote $CSV"
