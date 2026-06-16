#!/usr/bin/env bash
# run.sh — Faasm-like WordCount demo sweep. Builds the WASM module (wasm32-wasip1)
# + AOT-compiles it, then sweeps corpus size × map fan-out N, one fresh process
# per cell (clean per-N peak RSS), into results.csv.
#
# Each map/reduce stage runs as a fresh wasmtime instance (Faaslet); state moves
# through Redis (serialized). Requires: rustc (+wasm32-wasip1), wasmtime, a Redis
# on 127.0.0.1:6379, python3 redis.
#
# Columns: size_mb,workers,topo,e2e_ms,throughput_mb_s,words_per_s,peak_mem_mb,
#          state_kv_mb,total_occurrences
#
# Usage: ./run.sh ["50 500 1000"] ["1 2 4 8 16"]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../../.." && pwd)"

SIZES="${1:-50 500 1000}"
N_LIST="${2:-1 2 4 8 16}"
CSV="$HERE/results.csv"
declare -A CORPUS=( [50]=corpus_large.txt [500]=corpus_xlarge.txt [1000]=corpus.txt )

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }

echo "[faasm-demo] building wc.wasm + wc.cwasm..."
rustc --target wasm32-wasip1 -O "$HERE/wc.rs" -o "$HERE/wc.wasm" 2>&1 | tail -2
wasmtime compile "$HERE/wc.wasm" -o "$HERE/wc.cwasm" 2>&1 | tail -1

first=1; : > "$CSV"
echo "[faasm-demo] sizes=[$SIZES] N=[$N_LIST]"
for s in $SIZES; do
  corpus="$WAS_ROOT/TestData/${CORPUS[$s]}"
  [ -f "$corpus" ] || { echo "  SKIP (missing): $corpus"; continue; }
  for N in $N_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    WC_CORPUS="$corpus" MAPPER_NUM="$N" python3 "$HERE/driver.py" "$N" $hdr | tee -a "$CSV"
    first=0
  done
done
echo "[faasm-demo] wrote $CSV"
