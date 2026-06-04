#!/usr/bin/env bash
# run.sh — sweep the MAP FAN-OUT (number of parallel wc_map worker processes)
# for a single-node word count and record wall time + correctness.
#
# Each fan-out N is generated as a flat native DAG (gen_wc.py) and run via
# `node-agent run` — no coordinator needed.  We record the wall time reported by
# the runner ("[run] completed in X.XXs") and the total_occurrences from the
# output file (which MUST be identical across all N — the corpus is partitioned,
# never duplicated, so the global count is fan-out-invariant).
#
# Why this sweep: each map worker is a separate OS process (fork/exec + wasmtime
# JIT + SHM mmap), the wave spawns all N at once with no cap, and the box has 8
# physical cores (16 HT).  So wall time should fall as N approaches core count,
# then flatten/rise from oversubscription + memory-bandwidth contention.
#
# Usage:
#   ./run.sh                                  # default: corpus_large, N=1..64, 3 reps
#   ./run.sh corpus_xlarge.txt                # bigger corpus
#   ./run.sh corpus_large.txt "1 4 8 16 32"   # custom fan-out list
#   ./run.sh corpus_large.txt "1 8 32" 5      # + repeats per N (median reported)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

CORPUS_NAME="${1:-corpus_large.txt}"
FANOUTS="${2:-1 2 4 8 16 32 64}"
REPEATS="${3:-3}"

CORPUS="$ROOT/TestData/$CORPUS_NAME"
NODE_AGENT="$ROOT/NodeAgent/target/release/node-agent"
SHM="/dev/shm/wc_fanout_sweep"
CSV="$HERE/results_${CORPUS_NAME%.txt}.csv"

log() { printf '\033[1;36m[fanout]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[fanout] %s\033[0m\n' "$*" >&2; exit 1; }

[ -f "$CORPUS" ]    || die "corpus not found: $CORPUS (generate with scripts/gen_corpora.sh)"
[ -x "$NODE_AGENT" ] || die "node-agent not built: $NODE_AGENT (run ./build.sh)"

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

corpus_bytes=$(wc -c < "$CORPUS")
echo "fanout,corpus_bytes,wall_ms_median,total_occurrences" > "$CSV"
log "corpus=$CORPUS_NAME ($corpus_bytes bytes)  fanouts=[$FANOUTS]  repeats=$REPEATS  cores=$(nproc)"
printf "%7s  %12s  %14s  %18s\n" "fanout" "corpus_B" "wall_ms(med)" "total_occurrences"

baseline_occ=""
for N in $FANOUTS; do
  dag="$HERE/.dag_n${N}.json"
  out="$HERE/.out_n${N}.txt"
  python3 "$HERE/gen_wc.py" "$N" "$CORPUS" "$out" "$SHM" > "$dag"

  walls=()
  occ=""
  crashed=0
  for _ in $(seq 1 "$REPEATS"); do
    rm -f "$SHM" "$out" 2>/dev/null || true
    # A single fan-out may fail (e.g. low fan-out on a large corpus OOMs the
    # ~0.5 GiB guest heap inside wc_map's read_all).  Record it and keep going
    # so the sweep shows exactly where the boundary is, rather than aborting.
    if ! res=$(cd "$ROOT" && "$NODE_AGENT" run --dag "$dag" 2>&1); then
      crashed=1
      reason=$(echo "$res" | grep -oiE "unreachable|capacity|exhausted|OOM" | head -1)
      break
    fi
    secs=$(echo "$res" | grep -oE "completed in [0-9]+\.[0-9]+s" | grep -oE "[0-9]+\.[0-9]+" | head -1)
    [ -n "$secs" ] || { echo "$res" | tail -5; die "could not parse wall time at fanout=$N"; }
    ms=$(awk "BEGIN{printf \"%d\", $secs*1000}")
    walls+=("$ms")
    occ=$(grep -oE "total_occurrences=[0-9]+" "$out" 2>/dev/null | grep -oE "[0-9]+" | head -1)
  done

  if [ "$crashed" = 1 ]; then
    echo "$N,$corpus_bytes,CRASH,${reason:-fail}" >> "$CSV"
    printf "%7s  %12s  %14s  %18s\n" "$N" "$corpus_bytes" "CRASH" "${reason:-fail}"
    continue
  fi

  wall=$(median "${walls[@]}")
  [ -z "$baseline_occ" ] && baseline_occ="$occ"
  flag=""
  [ "$occ" != "$baseline_occ" ] && flag="  <-- COUNT MISMATCH (expected $baseline_occ)"
  echo "$N,$corpus_bytes,$wall,${occ:-NA}" >> "$CSV"
  printf "%7s  %12s  %14s  %18s%s\n" "$N" "$corpus_bytes" "$wall" "${occ:-NA}" "$flag"
done

rm -f "$HERE"/.dag_n*.json "$HERE"/.out_n*.txt "$SHM" 2>/dev/null || true
log "wrote $CSV"
