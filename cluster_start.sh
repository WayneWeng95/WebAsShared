#!/usr/bin/env bash
# cluster_start.sh — from the coordinator (node 0), bring every WORKER node up to
# date and (re)start its node-agent over SSH, in parallel.  Replaces logging into
# each node by hand to run node_update.sh.
#
# Worker IPs are read from NodeAgent/agent.toml (cluster.ips, in node_id order;
# index 0 = coordinator and is skipped), so the cluster definition stays in one
# place.  On `start`, each worker runs node_update.sh (git pull → ./build.sh →
# start agent) DETACHED via `setsid` — so this works with any version of the
# remote node_update.sh, and the SSH session returns immediately.  The agent only
# comes up if the build succeeded (node_update.sh is `set -e`), so we then POLL
# each worker until its agent is up (or a build-timeout elapses) and report.
#
# Usage:
#   ./cluster_start.sh                 # update + (re)start all workers, verify
#   ./cluster_start.sh --only 1,3,5    # only these node_ids
#   ./cluster_start.sh --status        # git HEAD + agent up/down per worker
#   ./cluster_start.sh --stop          # stop the agent on every worker
#   ./cluster_start.sh --no-pull       # rebuild current tree, skip git pull
#
# Env:  REMOTE_DIR (default /opt/myapp/WebAsShared)
#       BUILD_TIMEOUT (default 360s — max time to wait for an agent to come up)
#
# NOTE: workers pull `origin/main`; commit AND push local changes first or they
# will build stale code (mismatched binaries → wrong results / wedged RDMA mesh).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; cd "$ROOT"

CONFIG="NodeAgent/agent.toml"
REMOTE_DIR="${REMOTE_DIR:-/opt/myapp/WebAsShared}"
BUILD_TIMEOUT="${BUILD_TIMEOUT:-360}"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new)
AGENT_PAT="node-agent (start|--config)"
REMOTE_LOG="/tmp/node_update.log"

C_GRN='\033[0;32m'; C_YEL='\033[1;33m'; C_RED='\033[0;31m'; C_NC='\033[0m'
info(){ echo -e "${C_GRN}[cluster]${C_NC} $*"; }
warn(){ echo -e "${C_YEL}[cluster]${C_NC} $*"; }
err (){ echo -e "${C_RED}[cluster]${C_NC} $*" >&2; }
ssh_to(){ ssh "${SSH_OPTS[@]}" "$1" "$2"; }

# ── Parse cluster.ips (node_id order; index 0 = coordinator) ──────────────────
mapfile -t ALL_IPS < <(grep -E '^[[:space:]]*ips[[:space:]]*=' "$CONFIG" \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+')
[ "${#ALL_IPS[@]}" -ge 2 ] || { err "could not parse cluster.ips from $CONFIG"; exit 1; }

# ── Args ──────────────────────────────────────────────────────────────────────
ACTION="start"; NO_PULL=0; ONLY=""
while [ $# -gt 0 ]; do
    case "$1" in
        --status)  ACTION="status" ;;
        --stop)    ACTION="stop" ;;
        --no-pull) NO_PULL=1 ;;
        --only)    ONLY="$2"; shift ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) err "unknown arg: $1"; exit 1 ;;
    esac
    shift
done

# Worker list: node_id → ip (skip coordinator at index 0, honor --only).
declare -A WANT=()
if [ -n "$ONLY" ]; then IFS=',' read -ra ids <<< "$ONLY"; for x in "${ids[@]}"; do WANT[$x]=1; done; fi
WORKER_IDS=(); declare -A WORKER_IP=()
for i in "${!ALL_IPS[@]}"; do
    [ "$i" -ge 1 ] || continue
    [ -z "$ONLY" ] || [ -n "${WANT[$i]:-}" ] || continue
    WORKER_IDS+=("$i"); WORKER_IP[$i]="${ALL_IPS[$i]}"
done
[ "${#WORKER_IDS[@]}" -gt 0 ] || { err "no worker nodes selected"; exit 1; }
info "coordinator: node 0 (${ALL_IPS[0]}) — workers: ${WORKER_IDS[*]} (${#WORKER_IDS[@]} nodes)"

agent_up(){ ssh_to "$1" "pgrep -f '$AGENT_PAT' >/dev/null && echo UP || echo DOWN" 2>/dev/null; }

# ── --status ──────────────────────────────────────────────────────────────────
if [ "$ACTION" = "status" ]; then
    for id in "${WORKER_IDS[@]}"; do
        ( s="$(ssh_to "${WORKER_IP[$id]}" "cd '$REMOTE_DIR' 2>/dev/null && printf 'HEAD=%s ' \"\$(git rev-parse --short HEAD 2>/dev/null)\"; pgrep -f '$AGENT_PAT' >/dev/null && echo agent=UP || echo agent=DOWN" 2>&1 | tr -d '\n')"
          printf "  node %-2s %-14s %s\n" "$id" "${WORKER_IP[$id]}" "${s:-UNREACHABLE}" ) &
    done; wait
    exit 0
fi

# ── --stop ────────────────────────────────────────────────────────────────────
if [ "$ACTION" = "stop" ]; then
    for id in "${WORKER_IDS[@]}"; do
        ( r="$(ssh_to "${WORKER_IP[$id]}" "pkill -f '$AGENT_PAT' && echo stopped || echo 'not running'" 2>&1)"
          printf "  node %-2s %-14s %s\n" "$id" "${WORKER_IP[$id]}" "$r" ) &
    done; wait
    exit 0
fi

# ── start: pre-flight (workers pull origin/main) ─────────────────────────────
if [ "$NO_PULL" -eq 0 ]; then
    git fetch -q origin main 2>/dev/null || true
    local_head="$(git rev-parse HEAD 2>/dev/null || echo '?')"
    origin_head="$(git rev-parse origin/main 2>/dev/null || echo '?')"
    [ -z "$(git status --porcelain 2>/dev/null | head -1)" ] || \
        warn "local tree has UNCOMMITTED changes — workers pull origin/main and won't see them."
    [ "$local_head" = "$origin_head" ] || \
        warn "local HEAD ($local_head) != origin/main ($origin_head) — push first so workers build the same code."
fi

# ── start: phase 1 — launch node_update.sh detached on every worker ──────────
# A worker whose current commit predates node_update.sh wouldn't have the script
# yet, so (unless --no-pull) we `git pull` SYNCHRONOUSLY first — that fetches the
# script and the latest code, and lets us see a pull failure — then run it
# detached with SKIP_PULL=1 so it goes straight to build + start.
if [ "$NO_PULL" -eq 1 ]; then
    LAUNCH="cd '$REMOTE_DIR' && { SKIP_PULL=1 setsid nohup ./node_update.sh </dev/null >'$REMOTE_LOG' 2>&1 & } && echo launched"
else
    LAUNCH="cd '$REMOTE_DIR' && git pull && { SKIP_PULL=1 setsid nohup ./node_update.sh </dev/null >'$REMOTE_LOG' 2>&1 & } && echo launched"
fi
info "launching update+build+start on ${#WORKER_IDS[@]} workers (detached)…"
for id in "${WORKER_IDS[@]}"; do
    ( r="$(ssh_to "${WORKER_IP[$id]}" "$LAUNCH" 2>&1)"
      [ "$r" = "launched" ] && printf "  node %-2s %-14s launched\n" "$id" "${WORKER_IP[$id]}" \
                            || printf "  ${C_RED}node %-2s %-14s launch FAILED: %s${C_NC}\n" "$id" "${WORKER_IP[$id]}" "$r" ) &
done; wait

# ── start: phase 2 — poll until each agent is up (build success) ─────────────
info "waiting for agents to come up (build timeout ${BUILD_TIMEOUT}s)…"
declare -A DONE=(); deadline=$(( $(date +%s) + BUILD_TIMEOUT ))
while :; do
    pending=0
    for id in "${WORKER_IDS[@]}"; do
        [ -n "${DONE[$id]:-}" ] && continue
        if [ "$(agent_up "${WORKER_IP[$id]}")" = "UP" ]; then
            DONE[$id]=1; info "node $id (${WORKER_IP[$id]}) — agent UP"
        else
            pending=$((pending+1))
        fi
    done
    [ "$pending" -eq 0 ] && break
    [ "$(date +%s)" -ge "$deadline" ] && break
    sleep 10
done

# ── summary ───────────────────────────────────────────────────────────────────
echo; info "── summary ──"; ok=0; fail=0
for id in "${WORKER_IDS[@]}"; do
    if [ -n "${DONE[$id]:-}" ]; then
        ok=$((ok+1)); printf "  ${C_GRN}node %-2s %-14s UP${C_NC}\n" "$id" "${WORKER_IP[$id]}"
    else
        fail=$((fail+1)); printf "  ${C_RED}node %-2s %-14s NOT UP (build slow/failed)${C_NC}\n" "$id" "${WORKER_IP[$id]}"
        echo "      last lines of $REMOTE_LOG:"
        ssh_to "${WORKER_IP[$id]}" "tail -n 8 '$REMOTE_LOG' 2>/dev/null" 2>/dev/null | sed 's/^/      /'
    fi
done
info "$ok up, $fail not up."
[ "$fail" -eq 0 ]
