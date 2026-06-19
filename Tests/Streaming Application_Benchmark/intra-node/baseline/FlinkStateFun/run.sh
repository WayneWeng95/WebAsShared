#!/usr/bin/env bash
# run.sh — drive the Flink StateFun streaming-application baseline against an
# already-deployed cluster (Kafka + StateFun + the two function deployments).
#
# Mirrors ../../run.sh (the WebAsShared run) at the harness level: it reuses the
# SAME deterministic event stream (../../gen_events.py) so the correctness gate
# is line-for-line comparable, then optionally runs a throughput ramp.
#
# Prereqs: deploy deployment/k8s/* (kafka.yaml, topics.yaml, flink-statefun.yaml,
# {mediareview,socialnetwork}-function.yaml) and have a reachable Kafka broker
# (e.g. the kafka-cluster-kafka-external-bootstrap nodePort). See README.md.
#
# Usage:
#   BROKER=<host:port> ./run.sh mediareview                       # gate only
#   BROKER=<host:port> ./run.sh socialnetwork 200000 10000        # gate
#   BROKER=<host:port> THROUGHPUT="400 600 800" ./run.sh mediareview
#   DRIVER=docker ./run.sh ...     # run the driver via streaming-example-producer
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUITE="$(cd "$HERE/../.." && pwd)"   # Tests/Streaming Application_Benchmark

WORKLOAD="${1:-mediareview}"
EVENTS="${2:-200000}"
USERS="${3:-${USERS:-10000}}"
SKEW="${SKEW:-0.0}"
BROKER="${BROKER:-127.0.0.1:9094}"
DRIVER="${DRIVER:-local}"   # local | docker
THROUGHPUT="${THROUGHPUT:-}"

case "$WORKLOAD" in mediareview|socialnetwork) ;; *) echo "unknown workload: $WORKLOAD" >&2; exit 1 ;; esac
log() { printf '\033[1;36m[statefun-stream]\033[0m %s\n' "$*"; }

CSV="$HERE/.events_${WORKLOAD}.csv"
log "gen_events: workload=$WORKLOAD events=$EVENTS users=$USERS skew=$SKEW"
python3 "$SUITE/gen_events.py" "$WORKLOAD" --events "$EVENTS" --users "$USERS" --skew "$SKEW" > "$CSV"

# bench <subcommand> [target]
# Runs stream_bench.py either with local python (DRIVER=local) or via the
# streaming-example-producer image (DRIVER=docker). For docker the events CSV is
# mounted at /app/events.csv; `seed` takes USERS instead of the CSV.
bench() {
  local sub="$1"; local target="${2:-}"
  local arg="$CSV"
  [ "$sub" = "seed" ] && arg="$USERS"
  if [ "$DRIVER" = "docker" ]; then
    local carg="$arg"
    [ "$sub" != "seed" ] && carg="/app/events.csv"
    docker run --rm --network host -v "$CSV:/app/events.csv" \
      streaming-example-producer "$sub" "$BROKER" "$WORKLOAD" "$carg" $target
  else
    python3 "$HERE/driver/stream_bench.py" "$sub" "$BROKER" "$WORKLOAD" "$arg" $target
  fi
}

log "seed $USERS keys -> $BROKER"
bench seed
sleep 5

log "replay $EVENTS events (correctness gate)"
bench replay
log "waiting for StateFun to drain..."
sleep 15

# The gate consumes the whole `responses` topic from earliest. For a clean,
# repeatable tally re-apply topics.yaml (or use a fresh deploy) before each run,
# the same way the vendored harness's refresh_topics() recreates topics between
# measurements — otherwise responses from a previous run inflate the counts.
log "==== correctness gate ===="
bench gate
gate_rc=$?

if [ -n "$THROUGHPUT" ]; then
  log "==== throughput ramp: $THROUGHPUT ===="
  printf "%12s %18s\n" "target_ev_s" "achieved_ev_s"
  for T in $THROUGHPUT; do
    res=$(bench throughput "$T" 2>&1)
    ach=$(echo "$res" | grep -oE "achieved_throughput=[0-9]+" | grep -oE "[0-9]+" | head -1)
    printf "%12s %18s\n" "$T" "${ach:-NA}"
    sleep 10
  done
fi

rm -f "$CSV" 2>/dev/null || true
exit "$gate_rc"
