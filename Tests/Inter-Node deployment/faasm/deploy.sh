#!/usr/bin/env bash
# deploy.sh — bring up the Faasm per-node agents across the cluster (track D).
# This IS the Faasm "deployment": Faasm has no orchestration framework, so the
# deployment is starting agent.py on every node. Coordinator then places Faaslets.
#
#   ./deploy.sh start    # start agent on THIS node; if run on the coordinator and
#                        # SSH to workers works, starts them too — else prints the
#                        # one command to run on each worker (SSH is blocked here).
#   ./deploy.sh status   # /health every node's agent
#   ./deploy.sh stop     # stop agent on THIS node (+ workers via SSH if available)
#   ./deploy.sh run SPEC # convenience: coordinator.py SPEC (after agents are up)
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
MODE="${1:-start}"
PIDFILE="/tmp/faasm_agent.pid"; LOGFILE="/tmp/faasm_agent.log"
log() { printf '\033[1;36m[faasm-deploy %s]\033[0m %s\n' "$(hostname)" "$*"; }
die() { printf '\033[1;31m[faasm-deploy] %s\033[0m\n' "$*" >&2; exit 1; }
ssh_ok() { timeout 6 ssh -o BatchMode=yes -o ConnectTimeout=5 "$1" true 2>/dev/null; }
am_coord() { ip -o addr show 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | grep -qx "$COORD_IP"; }
health() { curl -s --max-time 4 "http://$1:$AGENT_PORT/health" 2>/dev/null; }

start_local() {
  if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    log "agent already running (pid $(cat "$PIDFILE"))"; return
  fi
  log "starting agent on :$AGENT_PORT (log $LOGFILE)"
  AGENT_PORT="$AGENT_PORT" STAGE_DIR="$STAGE_DIR" nohup python3 "$HERE/agent.py" >"$LOGFILE" 2>&1 &
  echo $! > "$PIDFILE"
  for i in $(seq 1 20); do health 127.0.0.1 >/dev/null && { log "agent healthy"; return; }; sleep 0.3; done
  die "agent did not become healthy — see $LOGFILE"
}
stop_local() {
  [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null && log "stopped agent" || log "no local agent"
  rm -f "$PIDFILE"
}

case "$MODE" in
  start)
    start_local
    if am_coord; then
      for ip in $WORKER_IPS; do
        if ssh_ok "$ip"; then
          log "SSH ok → starting agent on $ip"
          ssh -o BatchMode=yes "$ip" "cd '$HERE' && ./deploy.sh start" || log "WARN: remote start on $ip failed"
        else
          log "no SSH to $ip — run on it:  cd '$HERE' && ./deploy.sh start"
        fi
      done
      sleep 1; "$0" status
    fi
    ;;
  status)
    for ip in "$COORD_IP" $WORKER_IPS; do
      h="$(health "$ip")"; [ -n "$h" ] && echo "  ✓ $ip  $h" || echo "  ✗ $ip  (agent down)"
    done
    ;;
  stop)
    stop_local
    am_coord && for ip in $WORKER_IPS; do ssh_ok "$ip" && ssh -o BatchMode=yes "$ip" "cd '$HERE' && ./deploy.sh stop" || true; done
    ;;
  run)
    shift; [ -n "${1:-}" ] || die "usage: ./deploy.sh run <job-spec.json> [--reps N]"
    exec python3 "$HERE/coordinator.py" "$@"
    ;;
  *) die "usage: ./deploy.sh {start|status|stop|run <spec>}" ;;
esac
