#!/usr/bin/env bash
# run.sh — WebAsShared TeraSort sweep (intra-node, shared-memory page-chain).
#
# Sweeps (input size × shuffle fan-out N) over the native terasort DAG and writes
# results.csv in the shared column shape used across all four systems:
#
#     size_mb,workers,topo,compute_ms,throughput_mb_s,records_per_s,peak_mem_mb,reps,checksum
#
# - compute_ms      : "[DAG][timing] TOTAL compute" (staging EXCLUDED, per TEST_PLAN).
# - throughput_mb_s : size_mb / (compute_ms/1000).
# - records_per_s   : total_records / (compute_ms/1000).
# - peak_mem_mb     : peak (Σ PRIVATE RSS of host tree + SHM bump offset), 50 ms
#                     out-of-band sampler — the billable-memory high-water the way
#                     metrics.rs defines it (private RSS + SHM counted once).
# - reps            : repeats per cell; compute_ms/throughput are the MEDIAN.
# - checksum        : "<total_records>:<total_keysum>" — FAN-OUT-INVARIANT. The
#                     whole dataset crosses the N×N shuffle, so dropping or
#                     duplicating any record changes one of these; the harness
#                     flags any mismatch across N (and also checks every range is
#                     internally sorted and that ranges are globally ordered).
#
# Usage:
#   ./run.sh                          # default: 50 500 1000 MB, N=1..16, 5 reps
#   ./run.sh "50"                     # one size
#   ./run.sh "50 500" "1 2 4 8 16" 5  # sizes, fan-outs, reps
# Env: TS_WASM (guest module, e.g. AOT .cwasm), TS_CSV (output csv path).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"          # → WebAsShared/

SIZES="${1:-50 500 1000}"
FANOUTS="${2:-1 2 4 8 16}"
REPEATS="${3:-5}"
TOPO="intra-shm"

NODE_AGENT="$ROOT/node-agent"
[ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
HOST_BIN="$ROOT/Executor/target/release/host"
SHM="/dev/shm/ts_appbench"
CSV="${TS_CSV:-$HERE/results.csv}"
OUT_PREFIX="$HERE/.ts_out"
DATA_DIR="$ROOT/TestData"

log() { printf '\033[1;36m[ts]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[ts] %s\033[0m\n' "$*" >&2; exit 1; }

[ -x "$NODE_AGENT" ] || die "node-agent not built (run ./build.sh)"
[ -x "$HOST_BIN" ]   || die "host executor not built: $HOST_BIN (run ./build.sh)"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# Background peak-memory sampler (see WordCount/run.sh for the metrics.rs rationale).
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

# Aggregate the N per-range summaries into "records keysum sorted ordered".
#   records = Σ records,  keysum = Σ keysum,  sorted = 1 iff every range sorted,
#   ordered = 1 iff last(range j) <= first(range j+1) for all j (global order).
aggregate_parts() {
  python3 - "$@" <<'PY'
import sys
records = keysum = 0
allsorted = 1
ranges = {}
for path in sys.argv[1:]:
    try:
        line = open(path).read().strip()
    except OSError:
        continue
    if not line:
        continue
    kv = dict(tok.split("=", 1) for tok in line.split() if "=" in tok)
    records += int(kv.get("records", 0))
    keysum  += int(kv.get("keysum", 0))
    if kv.get("sorted") != "1":
        allsorted = 0
    if int(kv.get("records", 0)) > 0:
        ranges[int(kv["range"])] = (kv.get("first", ""), kv.get("last", ""))
ordered = 1
prev_last = None
for j in sorted(ranges):
    first, last = ranges[j]
    if prev_last is not None and first < prev_last:
        ordered = 0
    prev_last = last
print(records, keysum, allsorted, ordered)
PY
}

echo "size_mb,workers,topo,compute_ms,throughput_mb_s,records_per_s,peak_mem_mb,reps,checksum" > "$CSV"
log "root=$ROOT  sizes=[$SIZES]MB  fanouts=[$FANOUTS]  reps=$REPEATS  cores=$(nproc)  wasm=${TS_WASM:-jit}"
printf "%8s %4s %12s %9s %11s %9s %s\n" "size_MB" "N" "compute_ms" "MB/s" "Mrec/s" "peak_MB" "checksum"

for SIZE in $SIZES; do
  REC="$DATA_DIR/terasort_${SIZE}mb.dat"
  if [ ! -f "$REC" ]; then
    log "generating $REC (${SIZE} MB) ..."
    python3 "$HERE/gen_records.py" "$SIZE" "$REC" || die "gen_records failed for ${SIZE}MB"
  fi
  bytes=$(wc -c < "$REC")
  size_mb=$(awk "BEGIN{printf \"%.0f\", $bytes/1048576}")

  base_ck=""
  for N in $FANOUTS; do
    dag="$HERE/.dag_n${N}.json"
    python3 "$HERE/gen_dag.py" "$N" "$REC" "$OUT_PREFIX" "$SHM" > "$dag"

    ms_list=(); peak_list=(); ck=""; crashed=0; reason=""
    for _ in $(seq 1 "$REPEATS"); do
      rm -f "$SHM" "$OUT_PREFIX".part* 2>/dev/null || true
      peakfile="$(mktemp)"
      sample_peak "$peakfile" & sampler=$!
      res=$(cd "$ROOT" && "$NODE_AGENT" run "$dag" 2>&1); rc=$?
      kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null || true
      if [ $rc -ne 0 ]; then
        crashed=1
        reason=$(echo "$res" | grep -oiE "unreachable|capacity|exhausted|OOM|out of memory" | head -1)
        rm -f "$peakfile"; break
      fi
      ms=$(echo "$res" | grep -oE "TOTAL compute: [0-9]+\.[0-9]+" | grep -oE "[0-9]+\.[0-9]+" | head -1)
      [ -n "$ms" ] || { echo "$res" | tail -8; rm -f "$peakfile"; die "no TOTAL compute at N=$N"; }
      ms_list+=("$ms")
      peak_kb=$(cat "$peakfile" 2>/dev/null || echo 0); rm -f "$peakfile"
      peak_list+=("$peak_kb")
      read recs ksum srt ordd < <(aggregate_parts "$OUT_PREFIX".part*)
      ck="${recs}:${ksum}"
      [ "$srt" = 1 ] || { log "WARN N=$N: a range is NOT sorted"; }
      [ "$ordd" = 1 ] || { log "WARN N=$N: ranges NOT globally ordered"; }
    done

    if [ "$crashed" = 1 ]; then
      echo "$size_mb,$N,$TOPO,CRASH,,,,${REPEATS},${reason:-fail}" >> "$CSV"
      printf "%8s %4s %12s %9s %11s %9s %s\n" "$size_mb" "$N" "CRASH" "-" "-" "-" "${reason:-fail}"
      continue
    fi

    med_ms=$(median "${ms_list[@]}")
    med_peak_kb=$(median "${peak_list[@]}")
    peak_mb=$(awk "BEGIN{printf \"%.1f\", $med_peak_kb/1024}")
    mbps=$(awk "BEGIN{printf \"%.1f\", $size_mb/($med_ms/1000)}")
    recs="${ck%%:*}"
    rps=$(awk "BEGIN{printf \"%.0f\", ${recs:-0}/($med_ms/1000)}")
    [ -z "$base_ck" ] && base_ck="$ck"
    flag=""; [ "$ck" != "$base_ck" ] && flag="  <-- CHECKSUM MISMATCH (want $base_ck)"
    echo "$size_mb,$N,$TOPO,$med_ms,$mbps,$rps,$peak_mb,$REPEATS,$ck" >> "$CSV"
    printf "%8s %4s %12s %9s %11.2f %9s %s%s\n" \
      "$size_mb" "$N" "$med_ms" "$mbps" "$(awk "BEGIN{print $rps/1e6}")" "$peak_mb" "$ck" "$flag"
  done
done

rm -f "$HERE"/.dag_n*.json "$OUT_PREFIX".part* "$SHM" 2>/dev/null || true
log "wrote $CSV"
