#!/usr/bin/env bash
# run.sh — sweep (image size x fanout degree) for the image-resize / fanout
# workload on OUR system, and record delivery throughput / latency in the same
# column shape as Roadrunner's fanout CSVs so the curves overlay.
#
# Drives OUR side only. The Roadrunner baselines (roadrunner / wasmedge / runc)
# run from their own WasmEdge + containerd stack — see
# ../../Benchmarks/TEST_PLAN.md § Workload 1b.
#
# Pipeline per cell: gen_dag.py builds a one-to-many fanout DAG (Input -> ingest
# -> Shuffle{Broadcast} -> N x WasmVoid{img_noop|img_resize} -> free); node-agent
# runs it; we parse "[DAG][timing] TOTAL compute: <ms>" (staging excluded).
#
# Prerequisites:
#   - Built system at the project root: ./build.sh -> node-agent, host, guest.wasm
#   - Image payloads:  python3 Tests/ImageResize_Fanout/gen_images.py
#   - Inter-node only: coordinator + worker started (NOT yet wired — see TOPO note).
#
# Usage:
#   Tests/ImageResize_Fanout/run.sh                          # default sweep
#   Tests/ImageResize_Fanout/run.sh "10" "1 10 20 40 60 80 100"   # sizes(MB), fanouts
#   FUNC=resize Tests/ImageResize_Fanout/run.sh              # per-task = single resize step
#
# Knobs (env):
#   FUNC=noop|resize   per-task work: noop = empty fn (pure transport, default) | resize = 1 step.
#                      Roadrunner's func_b is receive-and-return, so noop is the apples-to-apples.
#   TOPO=intra|inter   intra-node SHM (default). inter-node RDMA needs the 2-node DAG generator
#                      + a running cluster — not wired yet (errors out with a pointer).
#   REPS=N             repetitions per cell (default 5); median reported.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
NODE_AGENT="$ROOT/node-agent"
GEN_DAG="$HERE/gen_dag.py"
CSV="$HERE/results.csv"
IMG_DIR="$ROOT/TestData/fanout_img"

TOPO="${TOPO:-intra}"
FUNC="${FUNC:-noop}"

SIZES_MB="${1:-10}"
FANOUTS="${2:-1 10 20 40 60 80 100}"
REPS="${REPS:-5}"

log() { printf '\033[1;36m[img-fanout]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[img-fanout] %s\033[0m\n' "$*" >&2; exit 1; }

[ -x "$NODE_AGENT" ] || die "node-agent not found at $NODE_AGENT (run ./build.sh)"
[ -f "$GEN_DAG" ]    || die "gen_dag.py not found at $GEN_DAG"
if [ "$TOPO" = "inter" ]; then
  die "TOPO=inter not wired yet: needs the 2-node RDMA fanout DAG generator + a running cluster.
       Next step in TEST_PLAN.md § 1a. For now run the intra-node (SHM) sweep (TOPO=intra)."
fi

# Timing parsers over node-agent output. Wave lines look like:
#   [DAG][timing]   wave  2:     0.03 ms ( 0.0%)  1 node(s)  [fanout]
#   [DAG][timing]   TOTAL compute: 372.44 ms across 4 wave(s)
# We split three numbers: total compute, the broadcast (delivery) wave, and the
# worker wave — delivery is the zero-copy headline; worker is spawn+compute.
parse_compute_ms()  { echo "$1" | grep -oE 'TOTAL compute:[[:space:]]*[0-9.]+ ms' | grep -oE '[0-9.]+' | head -1; }
parse_delivery_ms() { echo "$1" | grep -E '\[fanout\]$' | grep -oE '[0-9.]+ ms' | grep -oE '[0-9.]+' | head -1; }
parse_worker_ms()   { echo "$1" | grep -E '\[w0[],]' | grep -oE '[0-9.]+ ms' | grep -oE '[0-9.]+' | head -1; }

median() { printf '%s\n' "$@" | sort -n | awk 'NF{a[NR]=$1} END{if(NR==0){print "NA"}else{print (NR%2)?a[(NR+1)/2]:(a[int(NR/2)]+a[int(NR/2)+1])/2}}'; }

echo "size_mb,fanout,topo,func,compute_ms,delivery_ms,worker_ms,throughput_mbps,reps" > "$CSV"
log "topo=$TOPO  func=$FUNC  sizes(MB)=[$SIZES_MB]  fanouts=[$FANOUTS]  reps=$REPS"

for mb in $SIZES_MB; do
  img="$IMG_DIR/${mb}MB.img"
  [ -f "$img" ] || die "payload $img missing — run: python3 Tests/ImageResize_Fanout/gen_images.py $mb"
  rel_img="TestData/fanout_img/${mb}MB.img"
  for fo in $FANOUTS; do
    # Fresh SHM file per cell; rm before each rep so no state accumulates (the
    # DAG has no FreeSlots — broadcast shares one page chain, see gen_dag.py).
    shm="/dev/shm/img_fanout_${FUNC}_${mb}MB_n${fo}"
    dag="$(python3 "$GEN_DAG" --n "$fo" --func "$FUNC" --image "$rel_img" --shm "$shm")"
    tot=(); del=(); wrk=()
    for _ in $(seq 1 "$REPS"); do
      rm -f "$shm"
      out="$(cd "$ROOT" && "$NODE_AGENT" run "$dag" 2>&1 | tr -d '\0')" || { echo "$out" | tail -8; die "run failed at ${mb}MB fanout=$fo"; }
      t="$(parse_compute_ms "$out")";  [ -n "${t:-}" ] && tot+=("$t")
      d="$(parse_delivery_ms "$out")"; [ -n "${d:-}" ] && del+=("$d")
      w="$(parse_worker_ms "$out")";   [ -n "${w:-}" ] && wrk+=("$w")
    done
    rm -f "$shm"
    med="$(median "${tot[@]:-}")"
    dmed="$(median "${del[@]:-}")"
    wmed="$(median "${wrk[@]:-}")"
    tput="$(awk -v mb="$mb" -v ms="$med" 'BEGIN{ if(ms+0>0) printf "%.2f", mb*1000.0/ms; else print "NA" }')"
    echo "${mb},${fo},${TOPO},${FUNC},${med},${dmed},${wmed},${tput},${REPS}" >> "$CSV"
    printf "  size=%4sMB fanout=%-4s -> total=%8s ms  delivery=%7s ms  worker=%8s ms  ~%s MB/s\n" \
           "$mb" "$fo" "$med" "$dmed" "$wmed" "$tput"
    rm -f "$dag"
  done
done

log "wrote $CSV"
