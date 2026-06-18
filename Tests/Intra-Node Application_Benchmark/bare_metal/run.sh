#!/usr/bin/env bash
# run.sh — BARE-METAL WordCount sweep (single process, N map threads).
#
# The reference floor for the WordCount suite: a plain multi-threaded Python
# program (wordcount.py), no framework / KVS / WASM / shared-memory substrate.
# Writes results.csv in the SAME column shape as the other systems so the rows
# drop straight into the cross-system comparison:
#
#   size_mb,workers,topo,compute_ms,throughput_mb_s,words_per_s,peak_mem_mb,reps,total_occurrences
#
# - compute_ms      : split + threaded map + merge (file read EXCLUDED, per the
#                     suite's methodology — only "input staging" is outside the timer).
# - throughput_mb_s : size_mb / (compute_ms/1000).
# - words_per_s     : total_occurrences / (compute_ms/1000).
# - peak_mem_mb     : peak Σ RSS of the python worker process, sampled out-of-band
#                     at 50 ms (no SHM term — bare metal has none). Same definition
#                     of "billable memory high-water" the other run.sh files use.
# - reps            : repeats per cell; compute_ms/throughput are the MEDIAN.
# - workers         : N = map-thread count.  topo = "bare-metal".
#
# total_occurrences is fan-out-invariant (the corpus is partitioned, never
# duplicated) and matches every other system's gate — the harness flags any drift.
#
# NOTE (the point of this baseline): Python's GIL serialises CPU-bound threads,
# so compute_ms is ~flat in N — N=1 is the real single-thread reference. That
# flatness is the finding; see README.md.
#
# Usage:
#   ./run.sh                                                   # 50MB + 500MB, N=1..16, 3 reps
#   ./run.sh "corpus_large.txt"                                # one corpus
#   ./run.sh "corpus_large.txt corpus_xlarge.txt corpus.txt" "1 2 4 8 16" 3
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"          # → WebAsShared/

CORPORA="${1:-corpus_large.txt corpus_xlarge.txt}"
FANOUTS="${2:-1 2 4 8 16}"
REPEATS="${3:-3}"
TOPO="bare-metal"

CSV="${WC_CSV:-$HERE/results.csv}"
WC="$HERE/wordcount.py"

log() { printf '\033[1;36m[bare]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[bare] %s\033[0m\n' "$*" >&2; exit 1; }

command -v python3 >/dev/null || die "python3 not found"
[ -f "$WC" ] || die "missing $WC"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# Background peak-RSS sampler: max Σ RSS (KB) of the python wordcount worker(s).
# Matches by the wordcount.py path so it never catches an unrelated python proc.
sample_peak() {
  local outfile="$1"; local peak=0
  echo 0 > "$outfile"
  while :; do
    local rss_kb
    rss_kb=$(for p in $(pgrep -f "bare_metal/wordcount.py" 2>/dev/null); do
               awk '/^VmRSS:/{print $2}' "/proc/$p/status" 2>/dev/null; done \
             | awk '{s+=$1} END{print s+0}')
    [ "${rss_kb:-0}" -gt "$peak" ] && { peak=$rss_kb; echo "$peak" > "$outfile"; }
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
    ms_list=(); peak_list=(); occ=""
    for _ in $(seq 1 "$REPEATS"); do
      peakfile="$(mktemp)"
      sample_peak "$peakfile" & sampler=$!
      res=$(python3 "$WC" "$CORPUS" "$N" 2>&1); rc=$?
      kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null || true
      if [ $rc -ne 0 ]; then
        echo "$res" | tail -6; rm -f "$peakfile"; die "wordcount.py failed (size=$size_mb N=$N)"
      fi
      ms=$(echo "$res"  | grep -oE "compute_ms=[0-9]+\.[0-9]+" | grep -oE "[0-9]+\.[0-9]+" | head -1)
      occ=$(echo "$res" | grep -oE "total_occurrences=[0-9]+" | grep -oE "[0-9]+" | head -1)
      [ -n "$ms" ] || { echo "$res" | tail -6; rm -f "$peakfile"; die "no compute_ms (size=$size_mb N=$N)"; }
      ms_list+=("$ms")
      peak_list+=("$(cat "$peakfile" 2>/dev/null || echo 0)"); rm -f "$peakfile"
    done

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

log "wrote $CSV"
