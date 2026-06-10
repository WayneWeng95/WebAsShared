#!/usr/bin/env bash
# run_pertask.sh — measure the wasmedge/native baselines the SAME WAY Roadrunner
# reports them, so the numbers line up directly with their published intra-node
# CSV (per-task latency; their throughput == 1/latency; plus RAM).
#
# For each fanout N: launch N resize tasks CONCURRENTLY, each self-timed with
# GNU `time -f '%e %M'` (per-task wall seconds + max RSS KB). We then report,
# across the N tasks:
#   latency_s   = MEAN per-task wall time   (their "total latency" column)
#   throughput  = 1 / latency_s             (their "total throughput" column)
#   rss_mb      = MEAN per-task max RSS      (their "ram" column, per task)
#   wave_ms     = wall time of the whole wave (our original metric, for context)
# Each task captures its OWN latency under fanout-N contention — so unlike their
# big-server numbers (flat per-task), ours will grow once our 16 cores saturate,
# which is exactly the hardware difference we want to surface.
#
# Usage:
#   Tests/ImageResize_Fanout/baseline/run_pertask.sh                          # default
#   Tests/ImageResize_Fanout/baseline/run_pertask.sh "1 10 20 40 60 80 100" 5
#   MODE=wasmedge|native|both (default both)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
CRATE="$HERE/resize"
IMG="$ROOT/TestData/fanout_img/10MB.img"
WASMEDGE="${WASMEDGE:-/opt/WasmEdge-0.11.2-Linux/bin/wasmedge}"
TIME=/usr/bin/time
CSV="$HERE/results_pertask.csv"

FANOUTS="${1:-1 10 20 40 60 80 100}"
REPS="${2:-5}"
MODE="${MODE:-both}"
SIZE_MB=10

log() { printf '\033[1;35m[rr-pertask]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[rr-pertask] %s\033[0m\n' "$*" >&2; exit 1; }

[ -f "$IMG" ]   || die "payload $IMG missing — python3 Tests/ImageResize_Fanout/gen_images.py 10"
[ -x "$TIME" ]  || die "GNU /usr/bin/time missing"
[ -x "$WASMEDGE" ] || die "wasmedge not at $WASMEDGE"

export PATH="$HOME/.cargo/bin:$PATH"
log "building resize (native + wasm32-wasip1)…"
( cd "$CRATE" && cargo build --release >/dev/null 2>&1 && cargo build --release --target wasm32-wasip1 >/dev/null 2>&1 ) || die "build failed"
NATIVE_BIN="$CRATE/target/release/resize"
WASM_BIN="$CRATE/target/wasm32-wasip1/release/resize.wasm"

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

# mean / median over a list of floats on stdin
mean()   { awk 'NF{s+=$1;n++} END{print (n? s/n : "NA")}'; }
median() { sort -n | awk 'NF{a[NR]=$1} END{print (NR? ((NR%2)?a[(NR+1)/2]:(a[int(NR/2)]+a[int(NR/2)+1])/2):"NA")}'; }

# Run one wave of N concurrent self-timed tasks; emit per-task "elapsed_s rss_kb" lines.
run_wave() {
  local mode="$1" n="$2" i
  rm -f "$TMP"/t_*
  for ((i=0; i<n; i++)); do
    if [ "$mode" = wasmedge ]; then
      "$TIME" -f '%e %M' "$WASMEDGE" "$WASM_BIN" < "$IMG" >/dev/null 2>"$TMP/t_$i" &
    else
      "$TIME" -f '%e %M' "$NATIVE_BIN" < "$IMG" >/dev/null 2>"$TMP/t_$i" &
    fi
  done
  wait
  cat "$TMP"/t_* 2>/dev/null   # each line: "<elapsed_s> <maxrss_kb>"
}

modes=(); case "$MODE" in both) modes=(wasmedge native);; *) modes=("$MODE");; esac

echo "size_mb,fanout,mode,latency_s,throughput_ops,rss_mb,wave_ms,reps" > "$CSV"
log "img=10MB fanouts=[$FANOUTS] reps=$REPS modes=[${modes[*]}]  (per-task: mean over N tasks, median over reps)"
for mode in "${modes[@]}"; do
  for fo in $FANOUTS; do
    lat_reps=(); rss_reps=(); wave_reps=()
    for ((r=0; r<REPS; r++)); do
      ws=$(date +%s%N)
      out="$(run_wave "$mode" "$fo")"
      we=$(date +%s%N)
      lat_reps+=("$(printf '%s\n' "$out" | awk '{print $1}' | mean)")     # mean per-task latency this rep
      rss_reps+=("$(printf '%s\n' "$out" | awk '{print $2}' | mean)")     # mean per-task RSS (kb) this rep
      wave_reps+=("$(awk -v s="$ws" -v e="$we" 'BEGIN{printf "%.1f",(e-s)/1000000}')")
    done
    lat="$(printf '%s\n' "${lat_reps[@]}" | median)"        # seconds
    rss_kb="$(printf '%s\n' "${rss_reps[@]}" | median)"
    wave="$(printf '%s\n' "${wave_reps[@]}" | median)"
    tput="$(awk -v l="$lat" 'BEGIN{print (l+0>0? 1.0/l : "NA")}')"
    rss_mb="$(awk -v k="$rss_kb" 'BEGIN{print (k+0>0? k/1024.0 : "NA")}')"
    printf '%s,%s,%s,%.4f,%.3f,%.2f,%s,%s\n' "$SIZE_MB" "$fo" "$mode" "$lat" "$tput" "$rss_mb" "$wave" "$REPS" >> "$CSV"
    printf "  %-8s fanout=%-4s -> lat=%7.3fs  tput=%6.2f ops/s  rss=%6.1f MB  wave=%sms\n" \
           "$mode" "$fo" "$lat" "$tput" "$rss_mb" "$wave"
  done
done
log "wrote $CSV"
