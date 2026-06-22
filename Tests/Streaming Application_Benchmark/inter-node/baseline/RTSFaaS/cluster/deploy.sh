#!/usr/bin/env bash
# deploy.sh — bring up the dedicated RTSFaaS per-node agents (rt-agent.py) across
# the cluster. This IS the RTSFaaS "deployment" in our setup: replaces the shipped
# scp+ssh run-application.sh. Coordinator (rt-coordinator.py) then places roles.
#
#   ./deploy.sh start    # start rt-agent on THIS node; if on coord and ssh works,
#                        # start workers too — else prints the per-node command.
#   ./deploy.sh status   # /health every node's agent
#   ./deploy.sh stop     # stop rt-agent on THIS node (+ workers via ssh if available)
#   ./deploy.sh image    # build rtfaas:1.0 here, save tar, scp+load to all nodes
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
MODE="${1:-start}"
PIDFILE="/tmp/rt_agent.pid"; LOGFILE="/tmp/rt_agent.log"
log() { printf '\033[1;35m[rt-deploy %s]\033[0m %s\n' "$(hostname)" "$*"; }
die() { printf '\033[1;31m[rt-deploy] %s\033[0m\n' "$*" >&2; exit 1; }
ssh_ok() { timeout 6 ssh -o BatchMode=yes -o ConnectTimeout=5 "$1" true 2>/dev/null; }
am_coord() { ip -o addr show 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | grep -qx "$COORD_IP"; }
health() { curl -s --max-time 4 "http://$1:$AGENT_PORT/health" 2>/dev/null; }
ALL_IPS="$(printf '%s\n' "$COORD_IP" $WORKER_IPS | sort -u)"

start_local() {
  if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    log "rt-agent already running (pid $(cat "$PIDFILE"))"; return; fi
  log "starting rt-agent on :$AGENT_PORT (log $LOGFILE)"
  AGENT_PORT="$AGENT_PORT" DOCKER="$DOCKER" nohup python3 "$HERE/rt-agent.py" >"$LOGFILE" 2>&1 &
  echo $! > "$PIDFILE"
  for i in $(seq 1 20); do health 127.0.0.1 >/dev/null && { log "healthy"; return; }; sleep 0.3; done
  die "rt-agent did not become healthy — see $LOGFILE"
}
stop_local() { [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null && log "stopped" || log "no local agent"; rm -f "$PIDFILE"; }

case "$MODE" in
  start)
    start_local
    if am_coord; then
      for ip in $ALL_IPS; do
        [ "$ip" = "$COORD_IP" ] && continue
        if ssh_ok "$ip"; then
          log "ssh $ip: starting rt-agent"
          ssh "$ip" "cd '$HERE' && ./deploy.sh start" || log "WARN: start failed on $ip"
        else
          log "ssh to $ip unavailable — run on $ip:  cd '$HERE' && ./deploy.sh start"
        fi
      done
    fi ;;
  stop)
    stop_local
    if am_coord; then for ip in $ALL_IPS; do [ "$ip" = "$COORD_IP" ] && continue
      ssh_ok "$ip" && ssh "$ip" "cd '$HERE' && ./deploy.sh stop" || true; done; fi ;;
  status)
    for ip in $ALL_IPS; do printf '%s: ' "$ip"; health "$ip" || echo "DOWN"; echo; done ;;
  image)
    command -v $DOCKER >/dev/null || die "docker missing"
    [ -f /opt/myapp/compare_system/RTSFaaS/Dockerfile ] || die "RTSFaaS Dockerfile not found"
    log "building $IMAGE (apply the 7 single-node fixes first — see ../single_node/README.md)"
    ( cd /opt/myapp/compare_system/RTSFaaS && $DOCKER build --platform=linux/amd64 -t "$IMAGE" . ) || die "build failed"
    TAR=/tmp/rtfaas_image.tar; $DOCKER save "$IMAGE" -o "$TAR"
    for ip in $ALL_IPS; do [ "$ip" = "$COORD_IP" ] && continue
      if ssh_ok "$ip"; then log "scp+load -> $ip"; scp "$TAR" "$ip:$TAR" && ssh "$ip" "$DOCKER load -i $TAR"
      else log "ssh to $ip unavailable — copy $TAR there and: $DOCKER load -i $TAR"; fi; done ;;
  *) die "usage: $0 {start|stop|status|image}" ;;
esac
