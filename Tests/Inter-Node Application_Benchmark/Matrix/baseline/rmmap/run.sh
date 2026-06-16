#!/usr/bin/env bash
# run.sh — RMMap (dmerge) ES SUMMA matrix-multiply sweep, single box.
#
# RMMap has no native matmul, so this is the ES port: r×c block multiply over
# Redis+pickle with each block a SEPARATE PROCESS (≈ Knative pod) — RMMap's
# per-function parallelism. ES needs no MITOSIS kernel module, so it runs
# natively in python3 (no docker / no dmerge Cython bindings required, unlike the
# wordcount/finra ES ports which reused dmerge's real handlers).
#
# Columns: size_n,workers,topo,e2e_ms,gflops,peak_mem_mb,kvs_ser_mb,checksum
#
# Requires: redis-server on 127.0.0.1:6379, numpy.
# Usage: ./run.sh ["1024 2048 4096"] ["1 2 4 8 16"]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../.." && pwd)"
GENMAT="$WAS_ROOT/Tests/Application_Benchmark/Matrix/gen_matrix.py"

SIZES="${1:-512 1024 2048}"
W_LIST="${2:-1 2 4 8 16}"
CSV="$HERE/results.csv"

redis-cli ping >/dev/null 2>&1 || { echo "redis not reachable on :6379" >&2; exit 1; }

first=1; : > "$CSV"
echo "[rmmap-es] sizes=[$SIZES] W=[$W_LIST]"
for N in $SIZES; do
  A="$WAS_ROOT/TestData/matrix/A_${N}.bin"; B="$WAS_ROOT/TestData/matrix/B_${N}.bin"
  if [ ! -f "$A" ] || [ ! -f "$B" ]; then python3 "$GENMAT" "$N" "$A" "$B" >/dev/null; fi
  for W in $W_LIST; do
    hdr=""; [ "$first" = 1 ] && hdr="--header"
    MAT_A="$A" MAT_B="$B" python3 "$HERE/driver.py" "$N" "$W" $hdr | tee -a "$CSV"
    first=0
  done
done
echo "[rmmap-es] wrote $CSV"
