#!/usr/bin/env bash
# run.sh — WebAsShared WordCount sweep (intra-node, shared-memory page-chain).
#
# Sweeps (corpus size × map fan-out N) over the native word_count DAG and writes
# results.csv in the shared column shape used across all four systems:
#
#     size_mb,workers,topo,compute_ms,throughput_mb_s,words_per_s,peak_mem_mb,reps,total_occurrences
#
# - compute_ms      : "[DAG][timing] TOTAL compute" (staging EXCLUDED, per TEST_PLAN).
# - throughput_mb_s : size_mb / (compute_ms/1000).
# - words_per_s     : total_occurrences / (compute_ms/1000).
# - peak_mem_mb     : peak (Σ PRIVATE RSS of the host process tree + SHM bump
#                     offset), sampled out-of-band at 50 ms (the node-agent metrics
#                     log is only 2 s cadence — too coarse for sub-second runs), so
#                     this is the true billable-memory high-water (private RSS + SHM,
#                     SHM counted once) the way metrics.rs defines it. Private RSS =
#                     VmRSS − RssShmem per process, so the shared SHM (which lands in
#                     every worker's VmRSS as RssShmem) is subtracted from each and
#                     added back exactly once via the bump offset.
# - reps            : repeats per cell; compute_ms/throughput are the MEDIAN.
#
# Counts are fan-out-invariant (the corpus is partitioned, never duplicated), so
# total_occurrences MUST be identical across every N for a given corpus — the
# harness flags any mismatch.
#
# Usage:
#   ./run.sh                                       # default: 50MB, N=1..16, 5 reps
#   ./run.sh "corpus_large.txt"                    # one corpus
#   ./run.sh "corpus_large.txt corpus_xlarge.txt"  # several corpora
#   ./run.sh "corpus_large.txt" "1 2 4 8 16" 5     # corpora, fan-outs, reps
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"          # → WebAsShared/

CORPORA="${1:-corpus_large.txt}"
FANOUTS="${2:-1 2 4 8 16}"
REPEATS="${3:-5}"
TOPO="intra-shm"

NODE_AGENT="$ROOT/node-agent"
[ -x "$NODE_AGENT" ] || NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
HOST_BIN="$ROOT/Executor/target/release/host"
SHM="/dev/shm/wc_appbench"
CSV="${WC_CSV:-$HERE/results.csv}"   # override for variant runs (e.g. results_aot.csv)
OUT="$HERE/.wc_out.txt"

log() { printf '\033[1;36m[wc]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[wc] %s\033[0m\n' "$*" >&2; exit 1; }

[ -x "$NODE_AGENT" ] || die "node-agent not built (run ./build.sh)"
[ -x "$HOST_BIN" ]   || die "host executor not built: $HOST_BIN (run ./build.sh)"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# Background peak-memory sampler: max(Σ PRIVATE RSS of host tree + SHM bump
# offset) in KB. Matches the executor host process(es) by binary path so it never
# picks up an unrelated `host` (e.g. the DNS util). Writes the running peak (KB)
# to $1. Mirrors metrics.rs: private RSS = VmRSS − RssShmem per process (the shared
# SHM is subtracted from every process), and the SHM is added back exactly once
# from the superblock bump offset (LE u32 at byte 4) — NOT VmRSS + file size, which
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

echo "size_mb,workers,topo,compute_ms,throughput_mb_s,words_per_s,peak_mem_mb,reps,total_occurrences" > "$CSV"
log "root=$ROOT  corpora=[$CORPORA]  fanouts=[$FANOUTS]  reps=$REPEATS  cores=$(nproc)"
printf "%8s %4s %12s %9s %11s %9s %6s\n" "size_MB" "N" "compute_ms" "MB/s" "Mwords/s" "peak_MB" "occ"

for CORPUS_NAME in $CORPORA; do
  CORPUS="$ROOT/TestData/$CORPUS_NAME"
  [ -f "$CORPUS" ] || { log "SKIP (missing): $CORPUS"; continue; }
  bytes=$(wc -c < "$CORPUS")
  size_mb=$(awk "BEGIN{printf \"%.0f\", $bytes/1048576}")

  base_occ=""
  for N in $FANOUTS; do
    dag="$HERE/.dag_n${N}.json"
    python3 "$HERE/gen_dag.py" "$N" "$CORPUS" "$OUT" "$SHM" > "$dag"

    ms_list=(); peak_list=(); occ=""; crashed=0
    for _ in $(seq 1 "$REPEATS"); do
      rm -f "$SHM" "$OUT" 2>/dev/null || true
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
      occ=$(grep -oE "total_occurrences=[0-9]+" "$OUT" 2>/dev/null | grep -oE "[0-9]+" | head -1)
    done

    if [ "$crashed" = 1 ]; then
      echo "$size_mb,$N,$TOPO,CRASH,,,,${REPEATS},${reason:-fail}" >> "$CSV"
      printf "%8s %4s %12s %9s %11s %9s %6s\n" "$size_mb" "$N" "CRASH" "-" "-" "-" "${reason:-fail}"
      continue
    fi

    med_ms=$(median "${ms_list[@]}")
    med_peak_kb=$(median "${peak_list[@]}")
    peak_mb=$(awk "BEGIN{printf \"%.1f\", $med_peak_kb/1024}")
    mbps=$(awk "BEGIN{printf \"%.1f\", $size_mb/($med_ms/1000)}")
    wps=$(awk "BEGIN{printf \"%.0f\", ${occ:-0}/($med_ms/1000)}")
    [ -z "$base_occ" ] && base_occ="$occ"
    flag=""; [ "$occ" != "$base_occ" ] && flag="  <-- COUNT MISMATCH (want $base_occ)"
    echo "$size_mb,$N,$TOPO,$med_ms,$mbps,$wps,$peak_mb,$REPEATS,${occ:-NA}" >> "$CSV"
    printf "%8s %4s %12s %9s %11.2f %9s %6s%s\n" \
      "$size_mb" "$N" "$med_ms" "$mbps" "$(awk "BEGIN{print $wps/1e6}")" "$peak_mb" "${occ:-NA}" "$flag"
  done
done

rm -f "$HERE"/.dag_n*.json "$OUT" "$SHM" 2>/dev/null || true
log "wrote $CSV"
