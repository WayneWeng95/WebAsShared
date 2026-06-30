#!/usr/bin/env bash
# prep_corpora.sh — cut the four weak-scaling WordCount corpora from corpus_4gb.txt.
#
# Weak scaling holds per-node bytes constant (BASE_MB/node) while the node count
# grows {1,3,6,9}, so the TOTAL corpus is BASE_MB × nodes. WordCount's heavy stages
# are placement:"all" — every node loads a 1/N line-aligned slice of the SAME file
# (SlotLoader::load_slice) — so we just need one file of the right TOTAL size per
# thread point, and each node ends up reading ≈ BASE_MB.
#
# Each cut is byte-truncated then trimmed back to the last newline so the file is
# line-aligned (load_slice splits on line boundaries; a half-line tail would skew
# the ground-truth word count).
#
# Output: TestData/scaling/corpus_weak_<TOTAL>mb.txt  for TOTAL ∈ {256,768,1536,2304}
# (BASE_MB=256 × {1,3,6,9}). Idempotent — skips a file that already exists at size.
#
# Run ONCE on node-0 (the coordinator); the coordinator RDMA-stages the chosen
# slice to the workers at submit time (placement:"all" + shared_inputs), so the
# cuts do not need to exist on the workers.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

SRC="${SRC:-$ROOT/TestData/corpus_4gb.txt}"
OUTDIR="$ROOT/TestData/scaling"
BASE_MB="${BASE_MB:-256}"
NODES_LIST="${NODES_LIST:-1 3 6 9}"
MiB=1048576

log() { printf '\033[1;36m[prep]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[prep] %s\033[0m\n' "$*" >&2; exit 1; }

[ -f "$SRC" ] || die "source corpus not found: $SRC (need a corpus ≥ largest cut)"
mkdir -p "$OUTDIR"

src_bytes=$(wc -c < "$SRC")
for n in $NODES_LIST; do
  total_mb=$(( BASE_MB * n ))
  want=$(( total_mb * MiB ))
  out="$OUTDIR/corpus_weak_${total_mb}mb.txt"
  [ "$want" -le "$src_bytes" ] || die "need ${total_mb} MB but $SRC is only $((src_bytes/MiB)) MB"

  if [ -f "$out" ]; then
    have=$(wc -c < "$out")
    # already line-aligned at ≈ want (within one line of the truncation point)
    if [ "$have" -le "$want" ] && [ "$have" -gt $(( want - 4096 )) ]; then
      log "skip ${total_mb} MB (exists: $((have/MiB)) MB)  $out"; continue
    fi
  fi

  log "cut ${total_mb} MB (n=$n × ${BASE_MB} MB) → $out"
  head -c "$want" "$SRC" > "$out"
  # trim trailing partial line: inspect only the last 64 KiB (lines are short, so a
  # newline is certainly within it) and truncate just past its last newline —
  # avoids scanning the whole multi-GB file.
  win=65536; have=$(wc -c < "$out"); base=$(( have > win ? have - win : 0 ))
  off_in_tail=$(tail -c "$win" "$out" | grep -boa $'\n' | tail -1 | cut -d: -f1)
  if [ -n "$off_in_tail" ]; then
    truncate -s $(( base + off_in_tail + 1 )) "$out"
  fi
  log "  → $(( $(wc -c < "$out") / MiB )) MB, $(wc -l < "$out") lines"
done

# ── strong-scaling fixed corpus (2 GiB) ───────────────────────────────────────
# One fixed total used at every thread point for strong scaling. 2 GiB both loads on
# a single node (16t baseline, ~5.6 GB peak under the 3.48 GiB SHM window) and keeps
# 144 workers fed (~14 MB/worker), so the per-node fixed cost amortizes (5.8× @ 144t).
STRONG_MB="${STRONG_MB:-2048}"
strong_out="$OUTDIR/corpus_strong_${STRONG_MB}mb.txt"
strong_want=$(( STRONG_MB * MiB ))
if [ -f "$strong_out" ] && [ "$(wc -c < "$strong_out")" -le "$strong_want" ] \
   && [ "$(wc -c < "$strong_out")" -gt $(( strong_want - 4096 )) ]; then
  log "skip strong ${STRONG_MB} MB (exists)  $strong_out"
else
  [ "$strong_want" -le "$src_bytes" ] || die "need ${STRONG_MB} MB but $SRC is only $((src_bytes/MiB)) MB"
  log "cut strong ${STRONG_MB} MB → $strong_out"
  head -c "$strong_want" "$SRC" > "$strong_out"
  win=65536; have=$(wc -c < "$strong_out"); base=$(( have > win ? have - win : 0 ))
  off_in_tail=$(tail -c "$win" "$strong_out" | grep -boa $'\n' | tail -1 | cut -d: -f1)
  [ -n "$off_in_tail" ] && truncate -s $(( base + off_in_tail + 1 )) "$strong_out"
  log "  → $(( $(wc -c < "$strong_out") / MiB )) MB, $(wc -l < "$strong_out") lines"
fi

log "scaling corpora ready in $OUTDIR/"
