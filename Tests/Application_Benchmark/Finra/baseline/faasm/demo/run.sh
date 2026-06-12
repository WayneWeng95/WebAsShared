#!/usr/bin/env bash
# run.sh — Faasm-like FINRA demo sweep. Builds finra.cwasm, runs each rule as a
# fresh wasmtime instance (Faaslet), state through Redis. One process per trades
# file → results.csv.  Needs rustc+wasm32-wasip1, wasmtime, Redis on :6379.
#   ./run.sh "10000 100000 1000000"
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS="$(cd "$HERE/../../../../../.." && pwd)"
SIZES="${1:-10000 100000 1000000}"; CSV="$HERE/results.csv"
redis-cli ping >/dev/null 2>&1 || { echo "redis not on :6379" >&2; exit 1; }

echo "[finra-faasm] building finra.wasm + .cwasm..."
rustc --target wasm32-wasip1 -O "$HERE/finra_wasi.rs" -o "$HERE/finra.wasm" 2>&1 | tail -1
wasmtime compile "$HERE/finra.wasm" -o "$HERE/finra.cwasm" 2>&1 | tail -1

first=1; : > "$CSV"
for n in $SIZES; do
  f="$WAS/TestData/finra/trades_${n}.csv"
  [ -f "$f" ] || { echo "skip (missing) $f"; continue; }
  hdr=""; [ "$first" = 1 ] && hdr="--header"
  WC_CORPUS="$f" python3 "$HERE/driver.py" $hdr | tee -a "$CSV"; first=0
done
echo "[finra-faasm] wrote $CSV"
