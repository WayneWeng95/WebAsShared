#!/usr/bin/env bash
# run.sh — PageSize experiment.
#
# Evaluates how the engine page-chain's PUT and GET latency scale with the
# **page size**, across the same transfer-size schedule as ../StateSync /
# ../Micro-Benchmarks.  It reuses the `shm_statesync` harness (which takes
# --page-bytes) — the real engine is NOT modified; this only rewrites the same
# page-chain PUT/GET with different page granularities to measure the effect.
#
#   page sizes      : 4 KiB (the engine default), 64 KiB, 2 MiB
#   transfer sizes  : 16 KiB, 1 MiB, 16 MiB, 128 MiB
#   metrics         : PUT and GET p50/p99 latency (µs) + GiB/s, per (page, size) cell
#
# Each run emits both engine rows (copy + zero-copy splice); PUT is identical for
# both (the write), the zero-copy GET is page-size agnostic (a pointer splice),
# and the copy GET grows with page/transfer size — so the plots use the copy row
# to show where page size actually matters.
#
# Usage:
#   ./run.sh build        # build the shm_statesync harness (release)
#   ./run.sh run          # sweep page x size -> results.csv
#   ./run.sh plot         # render figs/ from results.csv
#   ./run.sh all          # build + run + plot
#   ./run.sh run --iters 100 --warmup 20      # extra args pass through to the harness
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="$ROOT/Executor/target/release/examples/shm_statesync"
CSV="$HERE/results.csv"

PAGES=(4096 65536 2097152)                       # 4 KiB, 64 KiB, 2 MiB
SIZES=(16384 1048576 16777216 134217728)         # 16 KiB, 1 MiB, 16 MiB, 128 MiB

log() { printf '\033[1;36m[pagesize]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[pagesize] %s\033[0m\n' "$*" >&2; exit 1; }

build() {
  log "building shm_statesync (release)…"
  ( cd "$ROOT/Executor" && cargo build --release -p connect --example shm_statesync )
}

run() {
  [ -x "$BIN" ] || build
  log "sweep: pages={4KiB,64KiB,2MiB} x sizes={16KiB,1MiB,16MiB,128MiB} -> $CSV"
  local tmp; tmp="$(mktemp)"
  local first=1
  for pb in "${PAGES[@]}"; do
    "$BIN" --sizes "${SIZES[@]}" --page-bytes "$pb" "$@" --csv "$tmp" | grep -E "page=|engine" || true
    if [ "$first" = 1 ]; then cat "$tmp" > "$CSV"; first=0; else tail -n +2 "$tmp" >> "$CSV"; fi
  done
  rm -f "$tmp"
  log "wrote $CSV"
}

plot() {
  [ -f "$CSV" ] || die "no $CSV — run './run.sh run' first"
  python3 "$HERE/plot_pagesize.py" --csv "$CSV"
  python3 "$HERE/plot_pagesweep.py" --csv "$CSV"
}

case "${1:-help}" in
  build) build ;;
  run)   shift || true; run "$@" ;;
  plot)  plot ;;
  all)   build; run; plot ;;
  help|-h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' ;;
  *) die "unknown command '${1}' (try: build | run | plot | all | help)" ;;
esac
