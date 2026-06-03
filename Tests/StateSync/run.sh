#!/usr/bin/env bash
# run.sh — StateSync evaluation of OUR framework.
#
# Drives the same 1->1 state-delivery test case as the two micro-benchmarks in
# ../Micro-Benchmarks, but through the framework's REAL data-passing paths:
#
#   local   the engine page-chain (common::Superblock slot chains), via the
#           `shm_statesync` example.  Reports put/get p50/p99 latency + GiB/s
#           for the shm-copy-engine and shm-zerocopy-engine rows.
#   remote  the engine's real RemoteSend path (connect::RdmaRemote: MR memcpy +
#           one-sided RDMA WRITE + TCP done-signal), via the `rdma_latency`
#           example.  Reports one-way latency (RTT/2) + throughput.
#
# Both harnesses live in Executor/connect/examples and reuse the engine's own
# types/protocol, so the numbers are faithful to the framework (not a model).
# CSV schemas match ../Micro-Benchmarks so the rows are directly comparable.
#
# Usage
# -----
#   ./run.sh build                 # build both harnesses (release)
#   ./run.sh local                 # local engine page-chain put/get  -> results_local.csv
#                                  #   (PUT excludes page allocation by default; --include-alloc folds it in)
#   ./run.sh local --readers 8 --sizes "16384 1048576" --iters 100
#   ./run.sh loopback              # remote RDMA, server+client on this node -> results_remote.csv
#   ./run.sh remote-server [port]                  # node B (consumer): start RDMA echo server
#   ./run.sh remote-client <host> [port]           # node A (producer): time the ping-pong
#   ./run.sh plot                  # compare engine zero-copy vs StateSync-local baselines -> figs/
#   ./run.sh all                   # build + local + loopback + plot (single-node smoke of everything)
#
# Two-node remote run (the real cross-node measurement):
#   # on node B (e.g. 10.10.1.1):   ./run.sh remote-server 7900
#   # on node A (e.g. 10.10.1.2):   ./run.sh remote-client 10.10.1.1 7900
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
EXAMPLES="$ROOT/Executor/target/release/examples"
LOCAL_BIN="$EXAMPLES/shm_statesync"
RDMA_BIN="$EXAMPLES/rdma_latency"

LOCAL_CSV="$HERE/results_local.csv"
REMOTE_CSV="$HERE/results_remote.csv"
RDMA_PORT_DEFAULT=7900

log() { printf '\033[1;36m[statesync]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[statesync] %s\033[0m\n' "$*" >&2; exit 1; }

build() {
  log "building shm_statesync + rdma_latency (release)…"
  ( cd "$ROOT/Executor" && cargo build --release -p connect \
      --example shm_statesync --example rdma_latency )
  log "built -> $EXAMPLES"
}

need_local() { [ -x "$LOCAL_BIN" ] || build; }
need_rdma()  { [ -x "$RDMA_BIN" ]  || build; }

run_local() {
  need_local
  log "local engine page-chain put/get  (csv -> $LOCAL_CSV)"
  "$LOCAL_BIN" --csv "$LOCAL_CSV" "$@"
}

run_loopback() {
  need_rdma
  local port="${1:-$RDMA_PORT_DEFAULT}"
  log "remote RDMA loopback on 127.0.0.1:$port  (csv -> $REMOTE_CSV)"
  "$RDMA_BIN" server "$port" >/tmp/statesync_rdma_server.log 2>&1 &
  local srv=$!
  # wait until the server is listening (no fixed sleep)
  for _ in $(seq 1 40); do
    ss -ltn 2>/dev/null | grep -q ":$port" && break
    read -t 0.25 -u 9 _ 2>/dev/null || true
  done 9<>/dev/null
  "$RDMA_BIN" client 127.0.0.1 "$port" --csv "$REMOTE_CSV"
  wait "$srv" 2>/dev/null || true
  log "server log: /tmp/statesync_rdma_server.log"
}

run_remote_server() {
  need_rdma
  local port="${1:-$RDMA_PORT_DEFAULT}"
  log "RDMA echo server (node B / consumer) on :$port — start the client on node A"
  exec "$RDMA_BIN" server "$port"
}

run_remote_client() {
  need_rdma
  local host="${1:-}" port="${2:-$RDMA_PORT_DEFAULT}"
  [ -n "$host" ] || die "remote-client needs <host> (the node-B IP)"
  log "RDMA client (node A / producer) -> $host:$port  (csv -> $REMOTE_CSV)"
  "$RDMA_BIN" client "$host" "$port" --csv "$REMOTE_CSV"
}

run_plot() {
  [ -f "$LOCAL_CSV" ] || die "no $LOCAL_CSV yet — run './run.sh local' first"
  log "plotting engine zero-copy vs StateSync-local baselines -> $HERE/figs"
  python3 "$HERE/plot_compare.py" "$@"
}

run_plot_remote() {
  log "plotting Redis (remote) vs Shared memory + RDMA -> $HERE/figs"
  python3 "$HERE/plot_remote_compare.py" "$@"
}

summary() {
  echo
  log "==== results ===="
  [ -f "$LOCAL_CSV" ]  && { echo "-- local  ($LOCAL_CSV)";  column -t -s, "$LOCAL_CSV";  echo; }
  [ -f "$REMOTE_CSV" ] && { echo "-- remote ($REMOTE_CSV)"; column -t -s, "$REMOTE_CSV"; echo; }
}

cmd="${1:-help}"; shift || true
case "$cmd" in
  build)          build ;;
  local)          run_local "$@"; summary ;;
  loopback)       run_loopback "$@"; summary ;;
  remote-server)  run_remote_server "$@" ;;
  remote-client)  run_remote_client "$@"; summary ;;
  plot)           run_plot "$@" ;;
  plot-remote)    run_plot_remote "$@" ;;
  all)            build; run_local; run_loopback; run_plot; summary ;;
  summary)        summary ;;
  help|-h|--help)
    sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' ;;
  *) die "unknown command '$cmd' (try: build | local | loopback | remote-server | remote-client | plot | plot-remote | all | summary | help)" ;;
esac
