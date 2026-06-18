#!/usr/bin/env bash
# run.sh — WebAsShared matrix-multiply sweep (intra-node, shared-memory page-chain).
#
# Sweeps (matrix size N × block-grid workers) over the native SUMMA matrix DAG and
# writes results.csv in the shared column shape used across all four systems:
#
#     size_n,workers,topo,compute_ms,gflops,peak_mem_mb,reps,checksum
#
# - compute_ms : "[DAG][timing] TOTAL compute" (staging EXCLUDED, per TEST_PLAN).
# - gflops     : 2*N^3 / (compute_ms/1000) / 1e9   (one mul + one add per inner step).
# - peak_mem_mb: peak (Σ PRIVATE RSS of the host process tree + SHM bump offset),
#                sampled out-of-band at 50 ms (true billable-memory high-water, the
#                way metrics.rs defines it: private RSS + SHM, SHM counted once).
#                Private RSS = VmRSS − RssShmem per process, so the shared SHM
#                (which lands in every worker's VmRSS as RssShmem) is subtracted
#                from each and added back exactly once via the bump offset.
# - reps       : repeats per cell; compute_ms/gflops are the MEDIAN.
#
# Correctness gate: A,B have integer entries, so checksum (Σ of all C entries) is
# exact and fan-out-invariant — it MUST equal gen_matrix.py's value at every
# worker count for a given N. The harness flags any mismatch.
#
# Usage:
#   ./run.sh                                  # default: 1024 2048 4096, W=1..16, 3 reps
#   ./run.sh "1024"                           # one size
#   ./run.sh "1024 2048 4096" "1 2 4 8 16" 3  # sizes, workers, reps
# Env: MAT_WASM → guest module (.cwasm for AOT); MAT_CSV → output path.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"          # → WebAsShared/

SIZES="${1:-512 1024 2048}"
WORKERS="${2:-1 2 4 8 16}"
REPEATS="${3:-3}"
TOPO="intra-shm"

NODE_AGENT="$ROOT/node-agent"
[ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
HOST_BIN="$ROOT/Executor/target/release/host"
DATADIR="$ROOT/TestData/matrix"
SHM="/dev/shm/mat_appbench"
CSV="${MAT_CSV:-$HERE/results.csv}"   # override for variant runs (e.g. results_aot.csv)
OUT="$HERE/.mat_out.txt"

log() { printf '\033[1;36m[mat]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[mat] %s\033[0m\n' "$*" >&2; exit 1; }

[ -x "$NODE_AGENT" ] || die "node-agent not built (run ./build.sh)"
[ -x "$HOST_BIN" ]   || die "host executor not built: $HOST_BIN (run ./build.sh)"
mkdir -p "$DATADIR"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# Background peak-memory sampler: max(Σ PRIVATE RSS of host tree + SHM bump offset)
# in KB. Mirrors metrics.rs: private RSS = VmRSS − RssShmem per process (the shared
# SHM is subtracted from every process), and the SHM is added back exactly once from
# the superblock bump offset (LE u32 at byte 4) — NOT VmRSS + file size, which
# double-counts the shared segment (once per worker that has it resident, plus the
# explicit file add).
sample_peak() {
  local outfile="$1"; local peak=0
  echo 0 > "$outfile"
  while :; do
    local rss_kb shm_kb tot
    rss_kb=$(for p in $(pgrep -f "target/release/host" 2>/dev/null); do
               awk '/^VmRSS:/{r=$2} /^RssShmem:/{s=$2} END{if(r!="")print r-s}' \
                 "/proc/$p/status" 2>/dev/null; done \
             | awk '{s+=$1} END{print s+0}')
    shm_kb=0
    [ -f "$SHM" ] && shm_kb=$(( $(od -An -tu4 -j4 -N4 "$SHM" 2>/dev/null || echo 0) / 1024 ))
    tot=$(( rss_kb + shm_kb ))
    [ "$tot" -gt "$peak" ] && { peak=$tot; echo "$peak" > "$outfile"; }
    sleep 0.05
  done
}

echo "size_n,workers,topo,compute_ms,gflops,peak_mem_mb,reps,checksum" > "$CSV"
log "root=$ROOT  sizes=[$SIZES]  workers=[$WORKERS]  reps=$REPEATS  cores=$(nproc)"
printf "%7s %4s %12s %9s %9s %16s\n" "N" "W" "compute_ms" "GFLOP/s" "peak_MB" "checksum"

for N in $SIZES; do
  A="$DATADIR/A_${N}.bin"; B="$DATADIR/B_${N}.bin"
  if [ ! -f "$A" ] || [ ! -f "$B" ]; then
    log "generating $N×$N inputs"
    python3 "$HERE/gen_matrix.py" "$N" "$A" "$B" >/dev/null || die "gen_matrix failed N=$N"
  fi
  # Expected checksum (the gate) — O(N^2) colsum(A)·rowsum(B) == (A·B).sum().
  EXPECT=$(python3 -c "import numpy as np; N=$N; A=np.fromfile('$A').reshape(N,N); B=np.fromfile('$B').reshape(N,N); print(int(A.sum(0)@B.sum(1)))")

  for W in $WORKERS; do
    dag="$HERE/.dag_n${N}_w${W}.json"
    python3 "$HERE/gen_dag.py" "$W" "$N" "$A" "$B" "$OUT" "$SHM" > "$dag" 2>/dev/null \
      || { echo "$N,$W,$TOPO,SKIP,,,$REPEATS,grid" >> "$CSV"; printf "%7s %4s %12s\n" "$N" "$W" "SKIP(grid)"; continue; }

    ms_list=(); peak_list=(); ck=""; crashed=0; reason=""
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
      [ -n "$ms" ] || { echo "$res" | tail -8; rm -f "$peakfile"; die "no TOTAL compute at N=$N W=$W"; }
      ms_list+=("$ms")
      peak_kb=$(cat "$peakfile" 2>/dev/null || echo 0); rm -f "$peakfile"
      peak_list+=("$peak_kb")
      ck=$(grep -oE "checksum=[0-9-]+" "$OUT" 2>/dev/null | grep -oE "[0-9-]+" | head -1)
    done

    if [ "$crashed" = 1 ]; then
      echo "$N,$W,$TOPO,CRASH,,,${REPEATS},${reason:-fail}" >> "$CSV"
      printf "%7s %4s %12s %9s %9s %16s\n" "$N" "$W" "CRASH" "-" "-" "${reason:-fail}"
      continue
    fi

    med_ms=$(median "${ms_list[@]}")
    med_peak_kb=$(median "${peak_list[@]}")
    peak_mb=$(awk "BEGIN{printf \"%.1f\", $med_peak_kb/1024}")
    gflops=$(awk "BEGIN{printf \"%.2f\", 2.0*$N*$N*$N/($med_ms/1000.0)/1e9}")
    flag=""; [ "$ck" != "$EXPECT" ] && flag="  <-- COUNT MISMATCH (want $EXPECT)"
    echo "$N,$W,$TOPO,$med_ms,$gflops,$peak_mb,$REPEATS,${ck:-NA}" >> "$CSV"
    printf "%7s %4s %12s %9s %9s %16s%s\n" "$N" "$W" "$med_ms" "$gflops" "$peak_mb" "${ck:-NA}" "$flag"
  done
done

rm -f "$HERE"/.dag_n*.json "$OUT" "$SHM" 2>/dev/null || true
log "wrote $CSV"
