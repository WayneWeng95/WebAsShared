#!/usr/bin/env bash
# run-local.sh — stand up the Flink StateFun streaming baseline on THIS machine
# (docker-compose, no k8s) and run the correctness gate / throughput ramp.
#
# Reuses the same deterministic stream as the WebAsShared run (../../gen_events.py),
# so the gate is line-for-line comparable. The driver runs as a one-shot
# container on the compose network, talking to kafka-broker:9092 internally.
#
# Usage:
#   sudo ./run-local.sh mediareview                 # gate
#   sudo ./run-local.sh socialnetwork 200000 10000  # gate
#   THROUGHPUT="400 600" sudo ./run-local.sh mediareview
#   KEEP=1 sudo ./run-local.sh ...     # leave the stack up afterwards
#
# Needs docker (this script calls `docker` directly; run under sudo if your user
# is not in the docker group). DOCKER=... overrides the docker command.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUITE="$(cd "$HERE/../.." && pwd)"
cd "$HERE"

DOCKER="${DOCKER:-docker}"
PROJECT=flinkstream
NET="${PROJECT}_default"
COMPOSE="$DOCKER compose -p $PROJECT"

WORKLOAD="${1:-mediareview}"
EVENTS="${2:-200000}"
USERS="${3:-${USERS:-10000}}"
SKEW="${SKEW:-0.0}"
THROUGHPUT="${THROUGHPUT:-}"
BROKER="kafka-broker:9092"

case "$WORKLOAD" in mediareview|socialnetwork) ;; *) echo "unknown workload: $WORKLOAD" >&2; exit 1 ;; esac
log() { printf '\033[1;36m[local]\033[0m %s\n' "$*"; }

drive() { # drive <subcommand> [extra args...]
  $DOCKER run --rm --network "$NET" -v "$HERE/.events.csv:/app/events.csv" \
    streaming-example-producer "$@"
}

log "building images (statefun, mediareview, socialnetwork)"
$COMPOSE build || { echo "compose build failed"; exit 1; }
log "building producer image"
$DOCKER build -f docker/Dockerfile.producer -t streaming-example-producer . || exit 1

log "bringing up the stack"
$COMPOSE up -d || exit 1

log "waiting for the StateFun job to reach RUNNING..."
# The master publishes the Flink REST API on host port 8081; poll it for a
# RUNNING job (the in-cluster `flink list` CLI needs rest.address and is unset
# in this compose image).
ready=0
for i in $(seq 1 60); do
  if curl -s localhost:8081/jobs 2>/dev/null | grep -q '"status":"RUNNING"'; then
    ready=1; log "StateFun job RUNNING"; break
  fi
  sleep 5
done
[ "$ready" = 1 ] || { log "job did not start; recent worker logs:"; $COMPOSE logs --tail=40 worker; [ "${KEEP:-0}" = 1 ] || $COMPOSE down; exit 1; }

log "gen_events: $WORKLOAD events=$EVENTS users=$USERS skew=$SKEW"
python3 "$SUITE/gen_events.py" "$WORKLOAD" --events "$EVENTS" --users "$USERS" --skew "$SKEW" > "$HERE/.events.csv"

log "seed $USERS keys"
drive seed "$BROKER" "$WORKLOAD" "$USERS"
sleep 5
log "replay $EVENTS events"
drive replay "$BROKER" "$WORKLOAD" /app/events.csv
log "draining..."
sleep 20

log "==== correctness gate ===="
drive gate "$BROKER" "$WORKLOAD" /app/events.csv
gate_rc=$?

if [ -n "$THROUGHPUT" ]; then
  log "==== throughput ramp: $THROUGHPUT ===="
  printf "%12s %18s\n" "target_ev_s" "achieved_ev_s"
  for T in $THROUGHPUT; do
    res=$(drive throughput "$BROKER" "$WORKLOAD" /app/events.csv "$T" 2>&1)
    ach=$(echo "$res" | grep -oE "achieved_throughput=[0-9]+" | grep -oE "[0-9]+" | head -1)
    printf "%12s %18s\n" "$T" "${ach:-NA}"
  done
fi

rm -f "$HERE/.events.csv" 2>/dev/null || true
if [ "${KEEP:-0}" != 1 ]; then
  log "tearing down (KEEP=1 to leave it up)"
  $COMPOSE down
fi
exit "$gate_rc"
