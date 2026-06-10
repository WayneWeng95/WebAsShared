#!/usr/bin/env bash
# run_baseline.sh — Roadrunner-style fanout BASELINES on this host (same hardware
# as our system), for the image-resize/fanout comparison.
#
# These are the vanilla baselines, NOT Roadrunner's zero-copy transport:
#   wasmedge : N parallel `wasmedge resize.wasm < image` tasks (vanilla WASI)
#   native   : N parallel `./resize < image` tasks (native processes)
# Each task loads its OWN copy of the image (via stdin) — there is no shared
# zero-copy delivery, so the fanout cost grows with N. Contrast with our system,
# whose Shuffle{Broadcast} splices one shared page chain to all N workers.
# (Roadrunner's own RoadRunner/Embedded transport columns can't run on this
# host's containerd 2.x — use their published intra-node CSV for those.)
#
# Per-task work = the resize crate here, byte-for-byte the same downscale-0.5 as
# the guest `img_resize`, so only the delivery differs.
#
# Usage:
#   Tests/ImageResize_Fanout/baseline/run_baseline.sh                       # default sweep
#   Tests/ImageResize_Fanout/baseline/run_baseline.sh "1 10 20 40 60 80 100" 5
#   MODE=wasmedge ... | MODE=native ... | MODE=both (default)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
CRATE="$HERE/resize"
IMG="$ROOT/TestData/fanout_img/10MB.img"
WASMEDGE="${WASMEDGE:-/opt/WasmEdge-0.11.2-Linux/bin/wasmedge}"
CSV="$HERE/results_baseline.csv"

FANOUTS="${1:-1 10 20 40 60 80 100}"
REPS="${2:-5}"
MODE="${MODE:-both}"
SIZE_MB=10

log() { printf '\033[1;35m[rr-baseline]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[rr-baseline] %s\033[0m\n' "$*" >&2; exit 1; }

[ -f "$IMG" ] || die "payload $IMG missing — run: python3 Tests/ImageResize_Fanout/gen_images.py 10"

export PATH="$HOME/.cargo/bin:$PATH"
log "building resize (native + wasm32-wasip1)…"
( cd "$CRATE" && cargo build --release >/dev/null 2>&1 && cargo build --release --target wasm32-wasip1 >/dev/null 2>&1 ) \
  || die "build failed (need ~/.cargo rustup + wasm32-wasip1 target)"
NATIVE_BIN="$CRATE/target/release/resize"
WASM_BIN="$CRATE/target/wasm32-wasip1/release/resize.wasm"
[ -x "$WASMEDGE" ] || die "wasmedge not found at $WASMEDGE"

median() { printf '%s\n' "$@" | sort -n | awk 'NF{a[NR]=$1} END{if(NR==0){print "NA"}else{print (NR%2)?a[(NR+1)/2]:(a[int(NR/2)]+a[int(NR/2)+1])/2}}'; }

# Launch N parallel resize tasks, each piping the image into its own stdin.
# Echo the wall-clock milliseconds for the whole wave.
run_wave() {
  local mode="$1" n="$2" i start end
  start=$(date +%s%N)
  for ((i=0; i<n; i++)); do
    if [ "$mode" = "wasmedge" ]; then
      "$WASMEDGE" "$WASM_BIN" < "$IMG" >/dev/null 2>&1 &
    else
      "$NATIVE_BIN" < "$IMG" >/dev/null 2>&1 &
    fi
  done
  wait
  end=$(date +%s%N)
  awk -v s="$start" -v e="$end" 'BEGIN{ printf "%.3f", (e-s)/1000000 }'
}

modes=(); case "$MODE" in both) modes=(wasmedge native);; *) modes=("$MODE");; esac

echo "size_mb,fanout,mode,total_ms,throughput_mbps,reps" > "$CSV"
log "img=10MB fanouts=[$FANOUTS] reps=$REPS modes=[${modes[*]}]"
for mode in "${modes[@]}"; do
  for fo in $FANOUTS; do
    times=()
    for ((r=0; r<REPS; r++)); do times+=("$(run_wave "$mode" "$fo")"); done
    med="$(median "${times[@]}")"
    tput="$(awk -v mb="$SIZE_MB" -v ms="$med" 'BEGIN{ if(ms+0>0) printf "%.2f", mb*1000.0/ms; else print "NA" }')"
    echo "${SIZE_MB},${fo},${mode},${med},${tput},${REPS}" >> "$CSV"
    printf "  %-8s fanout=%-4s -> median=%9s ms  ~%s MB/s\n" "$mode" "$fo" "$med" "$tput"
  done
done
log "wrote $CSV"
