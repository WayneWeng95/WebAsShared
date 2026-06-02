#!/usr/bin/env bash
#
# deploy_backends.sh — provision the external state backends for the StateSync
# micro-benchmark (Redis + an S3-compatible object store via MinIO).
#
# Why native (not Docker):
#   This is a *latency* micro-benchmark.  Docker's default bridge networking
#   adds NAT/veth overhead (often 10-40 us round-trip) that would pollute the
#   "external in-memory remote (Redis)" and "external storage remote (S3)"
#   measurements.  We install native binaries bound directly to the experiment
#   network interface so the numbers reflect the store + network only.
#
# Topology (from NodeAgent/agent_*.toml, cluster.ips = ["10.10.1.2","10.10.1.1"]):
#   node 0 / coordinator = 10.10.1.2   -> compute node (producer/consumer)
#   node 1 / worker      = 10.10.1.1   -> remote state backends live here
#
# Typical usage
# -------------
#   # On the REMOTE backend node (10.10.1.1): bring up Redis + S3 for the
#   # "external remote" approaches.
#   ./deploy_backends.sh up
#
#   # On the COMPUTE node (10.10.1.2): a local Redis for the "local in-memory"
#   # approach, bound to loopback only.
#   ./deploy_backends.sh up --bind 127.0.0.1 --no-s3
#
#   # TWO-NODE EXPERIMENT, no SSH required — run one command per machine:
#   #   on the BACKEND node (e.g. 10.10.1.1):
#   ./deploy_backends.sh backend
#   #   on the COMPUTE node (e.g. 10.10.1.2), pointing at the backend IP:
#   ./deploy_backends.sh compute --remote 10.10.1.1
#   #   then on the COMPUTE node:
#   python3 bench.py --csv results.csv
#
#   # If passwordless SSH IS available, this does both sides from one host:
#   ./deploy_backends.sh two-node 10.10.1.1
#
#   ./deploy_backends.sh status
#   ./deploy_backends.sh down
#
# Each `up` writes a `backends.env` next to this script with the resolved
# endpoints + credentials, ready to `source` from the benchmark harness.

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNDIR="${RUNDIR:-$SCRIPT_DIR/.run}"
ENV_FILE="$SCRIPT_DIR/backends.env"

REDIS_PORT="${REDIS_PORT:-6379}"
S3_PORT="${S3_PORT:-9000}"
S3_CONSOLE_PORT="${S3_CONSOLE_PORT:-9001}"
S3_USER="${S3_USER:-minioadmin}"
S3_PASS="${S3_PASS:-minioadmin123}"        # MinIO requires >= 8 chars
S3_BUCKET="${S3_BUCKET:-statesync}"
S3_DATADIR="${S3_DATADIR:-/tmp/statesync-minio-data}"
REDIS_PASS="${REDIS_PASS:-}"               # empty = no auth (experiment net is private)

WANT_REDIS=1
WANT_S3=1
BIND_IP=""
REMOTE_HOST=""                             # backend node IP, used by `compute`
REDIS_DIR="${REDIS_DIR:-/tmp/statesync-redis}"   # off-repo dir; persistence is off anyway

# MinIO release binaries (static, no deps).
MINIO_URL="https://dl.min.io/server/minio/release/linux-amd64/minio"
MC_URL="https://dl.min.io/client/mc/release/linux-amd64/mc"
BIN_DIR="${BIN_DIR:-$RUNDIR/bin}"

# ── Logging helpers ───────────────────────────────────────────────────────────
log()  { printf '\033[1;34m[deploy]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[ ok  ]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[fail]\033[0m %s\n' "$*" >&2; exit 1; }

# ── Detect the experiment-network bind address ────────────────────────────────
detect_bind_ip() {
    # Prefer a 10.10.1.x address (CloudLab experiment net), else first non-loopback.
    local ip
    ip="$(ip -o -4 addr show 2>/dev/null | awk '{print $4}' | cut -d/ -f1 \
          | grep -E '^10\.10\.1\.' | head -1 || true)"
    if [[ -z "$ip" ]]; then
        ip="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
    fi
    [[ -n "$ip" ]] || die "could not auto-detect a bind IP; pass --bind <ip>"
    echo "$ip"
}

# ── Argument parsing ──────────────────────────────────────────────────────────
parse_flags() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --bind)         BIND_IP="$2";        shift 2;;
            --remote)       REMOTE_HOST="$2";    shift 2;;
            --redis-port)   REDIS_PORT="$2";     shift 2;;
            --s3-port)      S3_PORT="$2";        shift 2;;
            --console-port) S3_CONSOLE_PORT="$2";shift 2;;
            --password)     REDIS_PASS="$2";     shift 2;;
            --s3-user)      S3_USER="$2";        shift 2;;
            --s3-pass)      S3_PASS="$2";        shift 2;;
            --bucket)       S3_BUCKET="$2";      shift 2;;
            --no-redis)     WANT_REDIS=0;        shift;;
            --no-s3)        WANT_S3=0;           shift;;
            *) die "unknown flag: $1";;
        esac
    done
}

# ── Dependency install (best-effort, apt-based) ───────────────────────────────
ensure_redis_server() {
    if command -v redis-server >/dev/null 2>&1; then return; fi
    log "redis-server not found — installing via apt"
    if command -v apt-get >/dev/null 2>&1; then
        sudo apt-get update -y && sudo apt-get install -y redis-server redis-tools
    else
        die "redis-server missing and apt-get unavailable; install Redis manually"
    fi
}

ensure_minio() {
    mkdir -p "$BIN_DIR"
    if [[ ! -x "$BIN_DIR/minio" ]]; then
        log "downloading MinIO server -> $BIN_DIR/minio"
        curl -fsSL "$MINIO_URL" -o "$BIN_DIR/minio" || die "MinIO download failed"
        chmod +x "$BIN_DIR/minio"
    fi
    if [[ ! -x "$BIN_DIR/mc" ]]; then
        log "downloading MinIO client (mc) -> $BIN_DIR/mc"
        curl -fsSL "$MC_URL" -o "$BIN_DIR/mc" || die "mc download failed"
        chmod +x "$BIN_DIR/mc"
    fi
}

# ── Redis lifecycle ───────────────────────────────────────────────────────────
start_redis() {
    ensure_redis_server
    mkdir -p "$RUNDIR"
    local conf="$RUNDIR/redis.conf"
    local pidfile="$RUNDIR/redis.pid"
    local logfile="$RUNDIR/redis.log"

    # Pure in-memory configuration for benchmark fidelity:
    #   - persistence OFF: no RDB snapshots (save ""), no AOF (appendonly no),
    #     and RDB-on-shutdown disabled — writes never touch disk
    #   - noeviction so the store never silently drops benchmark state
    #   - dir lives off-repo (/tmp) so even a manual BGSAVE can't litter the tree
    #   - bind to the experiment IP + loopback; protected-mode off (private net)
    mkdir -p "$REDIS_DIR"
    local bind_line="bind $BIND_IP 127.0.0.1"
    [[ "$BIND_IP" == "127.0.0.1" ]] && bind_line="bind 127.0.0.1"
    cat > "$conf" <<EOF
$bind_line
port $REDIS_PORT
protected-mode no
daemonize no
pidfile $pidfile
dir $REDIS_DIR
save ""
appendonly no
rdb-del-sync-files yes
maxmemory-policy noeviction
tcp-backlog 511
proto-max-bulk-len 1gb
EOF
    [[ -n "$REDIS_PASS" ]] && echo "requirepass $REDIS_PASS" >> "$conf"

    if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
        warn "Redis already running (pid $(cat "$pidfile")) — restarting"
        stop_redis
    fi

    # The apt package starts a systemd-managed Redis on 127.0.0.1:6379 that
    # would collide with our purpose-built instance (custom bind + persistence
    # off).  Stop and disable it so the port is free and it won't auto-respawn.
    if command -v systemctl >/dev/null 2>&1; then
        for svc in redis-server redis; do
            if systemctl is-active --quiet "$svc" 2>/dev/null; then
                warn "stopping distro-managed '$svc' service (frees port $REDIS_PORT)"
                sudo systemctl stop "$svc" 2>/dev/null || true
                sudo systemctl disable "$svc" 2>/dev/null || true
            fi
        done
    fi

    log "starting Redis on $BIND_IP:$REDIS_PORT (persistence disabled)"
    nohup redis-server "$conf" >"$logfile" 2>&1 &
    echo $! > "$pidfile"

    # Health check.
    local auth=(); [[ -n "$REDIS_PASS" ]] && auth=(-a "$REDIS_PASS")
    for _ in $(seq 1 20); do
        if redis-cli -h "$BIND_IP" -p "$REDIS_PORT" "${auth[@]}" ping 2>/dev/null | grep -q PONG; then
            ok "Redis up — PONG from $BIND_IP:$REDIS_PORT"
            return 0
        fi
        sleep 0.25
    done
    die "Redis did not become healthy; see $logfile"
}

stop_redis() {
    local pidfile="$RUNDIR/redis.pid"
    if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
        kill "$(cat "$pidfile")" && ok "Redis stopped"
    else
        warn "no running Redis pidfile at $pidfile"
    fi
    rm -f "$pidfile"
}

# ── MinIO (S3) lifecycle ──────────────────────────────────────────────────────
start_minio() {
    ensure_minio
    mkdir -p "$RUNDIR" "$S3_DATADIR"
    local pidfile="$RUNDIR/minio.pid"
    local logfile="$RUNDIR/minio.log"

    if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
        warn "MinIO already running (pid $(cat "$pidfile")) — restarting"
        stop_minio
    fi

    log "starting MinIO on $BIND_IP:$S3_PORT (console :$S3_CONSOLE_PORT), data=$S3_DATADIR"
    MINIO_ROOT_USER="$S3_USER" MINIO_ROOT_PASSWORD="$S3_PASS" \
        nohup "$BIN_DIR/minio" server "$S3_DATADIR" \
            --address "$BIND_IP:$S3_PORT" \
            --console-address "$BIND_IP:$S3_CONSOLE_PORT" \
            >"$logfile" 2>&1 &
    echo $! > "$pidfile"

    for _ in $(seq 1 40); do
        if curl -fsS "http://$BIND_IP:$S3_PORT/minio/health/live" >/dev/null 2>&1; then
            ok "MinIO up — health/live OK at http://$BIND_IP:$S3_PORT"
            break
        fi
        sleep 0.25
    done
    curl -fsS "http://$BIND_IP:$S3_PORT/minio/health/live" >/dev/null 2>&1 \
        || die "MinIO did not become healthy; see $logfile"

    # Create the benchmark bucket.
    "$BIN_DIR/mc" alias set statesync "http://$BIND_IP:$S3_PORT" "$S3_USER" "$S3_PASS" >/dev/null
    "$BIN_DIR/mc" mb --ignore-existing "statesync/$S3_BUCKET" >/dev/null
    ok "bucket ready — s3://$S3_BUCKET"
}

stop_minio() {
    local pidfile="$RUNDIR/minio.pid"
    if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
        kill "$(cat "$pidfile")" && ok "MinIO stopped"
    else
        warn "no running MinIO pidfile at $pidfile"
    fi
    rm -f "$pidfile"
}

# ── backends.env emission ─────────────────────────────────────────────────────
write_env() {
    {
        echo "# Generated by deploy_backends.sh on $(hostname) — source me from the harness."
        echo "# bind IP for the backends started on THIS node:"
        echo "STATESYNC_BIND_IP=$BIND_IP"
        if [[ $WANT_REDIS -eq 1 ]]; then
            echo "REDIS_HOST=$BIND_IP"
            echo "REDIS_PORT=$REDIS_PORT"
            echo "REDIS_PASS=$REDIS_PASS"
        fi
        if [[ $WANT_S3 -eq 1 ]]; then
            echo "S3_ENDPOINT=http://$BIND_IP:$S3_PORT"
            echo "S3_ACCESS_KEY=$S3_USER"
            echo "S3_SECRET_KEY=$S3_PASS"
            echo "S3_BUCKET=$S3_BUCKET"
            echo "S3_REGION=us-east-1"
        fi
    } > "$ENV_FILE"
    ok "wrote endpoints -> $ENV_FILE"
    echo
    cat "$ENV_FILE"
}

# Merged env for the two-node experiment: remote rows (redis-remote, s3) point
# at the backend node; the redis-local row points at loopback on this node.
write_two_node_env() {
    local remote="$1"
    {
        echo "# Generated by deploy_backends.sh two-node on $(hostname)."
        echo "# remote rows (redis-remote, s3) -> $remote ; redis-local -> loopback"
        echo "REDIS_HOST=$remote"
        echo "REDIS_PORT=$REDIS_PORT"
        echo "REDIS_PASS=$REDIS_PASS"
        echo "REDIS_LOCAL_HOST=127.0.0.1"
        echo "REDIS_LOCAL_PORT=$REDIS_PORT"
        echo "S3_ENDPOINT=http://$remote:$S3_PORT"
        echo "S3_ACCESS_KEY=$S3_USER"
        echo "S3_SECRET_KEY=$S3_PASS"
        echo "S3_BUCKET=$S3_BUCKET"
        echo "S3_REGION=us-east-1"
    } > "$ENV_FILE"
    ok "wrote two-node endpoints -> $ENV_FILE"
    echo
    cat "$ENV_FILE"
}

# ── Top-level commands ────────────────────────────────────────────────────────
cmd_up() {
    parse_flags "$@"
    [[ -z "$BIND_IP" ]] && BIND_IP="$(detect_bind_ip)"
    log "bind IP = $BIND_IP   (redis=$WANT_REDIS s3=$WANT_S3)"
    [[ $WANT_REDIS -eq 1 ]] && start_redis
    [[ $WANT_S3    -eq 1 ]] && start_minio
    write_env
    ok "all requested backends are up"
}

cmd_down() {
    parse_flags "$@"
    stop_minio || true
    stop_redis || true
    ok "teardown complete"
}

cmd_status() {
    parse_flags "$@"
    [[ -z "$BIND_IP" ]] && BIND_IP="$(detect_bind_ip)"
    local auth=(); [[ -n "$REDIS_PASS" ]] && auth=(-a "$REDIS_PASS")
    if redis-cli -h "$BIND_IP" -p "$REDIS_PORT" "${auth[@]}" ping 2>/dev/null | grep -q PONG; then
        ok "Redis  : UP   ($BIND_IP:$REDIS_PORT)"
    else
        warn "Redis  : DOWN ($BIND_IP:$REDIS_PORT)"
    fi
    if curl -fsS "http://$BIND_IP:$S3_PORT/minio/health/live" >/dev/null 2>&1; then
        ok "MinIO  : UP   (http://$BIND_IP:$S3_PORT)"
    else
        warn "MinIO  : DOWN (http://$BIND_IP:$S3_PORT)"
    fi
}

# Copy this script to a remote host and run a command there over SSH.
#   deploy_backends.sh remote <host> <up|down|status> [flags...]
cmd_remote() {
    local host="${1:-}"; shift || true
    [[ -n "$host" ]] || die "usage: deploy_backends.sh remote <host> <up|down|status> [flags]"
    local sub="${1:-up}"; shift || true
    local remote_path="/tmp/deploy_backends.sh"
    log "copying script to $host:$remote_path"
    scp -q "${BASH_SOURCE[0]}" "$host:$remote_path"
    log "running '$sub $*' on $host"
    # shellcheck disable=SC2029
    ssh "$host" "bash $remote_path $sub $*"
}

# One-shot two-node experiment setup, driven from the compute node:
#   1. start a loopback-only Redis here (the redis-local row)
#   2. deploy Redis + S3 on <remote-host> (the redis-remote + s3 rows)
#   3. write a merged backends.env pointing each row at the right place
#
#   deploy_backends.sh two-node <remote-host> [up|down] [flags...]
cmd_two_node() {
    local remote="${1:-}"; shift || true
    [[ -n "$remote" ]] || die "usage: deploy_backends.sh two-node <remote-host> [up|down] [flags]"
    local action="up"
    if [[ "${1:-}" == "up" || "${1:-}" == "down" ]]; then action="$1"; shift; fi
    parse_flags "$@"

    local remote_path="/tmp/deploy_backends.sh"

    if [[ "$action" == "down" ]]; then
        log "two-node teardown"
        BIND_IP="127.0.0.1"; stop_redis || true
        scp -q "${BASH_SOURCE[0]}" "$remote:$remote_path" 2>/dev/null || true
        ssh "$remote" "bash $remote_path down" || warn "remote teardown on $remote failed"
        ok "two-node teardown complete"
        return
    fi

    # 1. Local loopback Redis for the redis-local row (no S3 on the compute node).
    log "two-node: starting loopback Redis on this (compute) node"
    BIND_IP="127.0.0.1"
    start_redis

    # 2. Remote Redis + S3 on the backend node, bound to <remote-host> so the
    #    endpoints match exactly what this node will dial.
    log "two-node: deploying Redis + S3 on $remote"
    scp -q "${BASH_SOURCE[0]}" "$remote:$remote_path"
    ssh "$remote" "bash $remote_path up --bind $remote $*" \
        || die "remote deploy on $remote failed"

    # 3. Merged endpoints for the harness.
    write_two_node_env "$remote"
    ok "two-node setup complete"
    log "verify:  ./deploy_backends.sh status   &&   ssh $remote 'bash $remote_path status'"
    log "run:     python3 bench.py --csv results.csv"
}

# ── Per-node role commands (no SSH; run one on each machine) ──────────────────
#
# Use these when passwordless SSH between nodes is unavailable.  Run `backend`
# on the storage node and `compute` on the compute node; then just
# `python3 bench.py` on the compute node.

# BACKEND node: bring up Redis + S3, bound to the experiment NIC.
#   ./deploy_backends.sh backend [--bind <ip>]
cmd_backend() {
    parse_flags "$@"
    [[ -z "$BIND_IP" ]] && BIND_IP="$(detect_bind_ip)"
    log "backend node — Redis + S3 on $BIND_IP"
    start_redis
    start_minio
    write_env
    ok "backend ready — Redis $BIND_IP:$REDIS_PORT, S3 http://$BIND_IP:$S3_PORT"
    log "now on the COMPUTE node run: ./deploy_backends.sh compute --remote $BIND_IP"
}

# COMPUTE node: bring up a loopback Redis (redis-local row) and write a merged
# backends.env that points the remote rows at the backend node.
#   ./deploy_backends.sh compute --remote <backend-ip>
cmd_compute() {
    parse_flags "$@"
    [[ -n "$REMOTE_HOST" ]] || die "usage: deploy_backends.sh compute --remote <backend-ip>"
    BIND_IP="127.0.0.1"
    WANT_S3=0
    log "compute node — loopback Redis (redis-local row); remote rows -> $REMOTE_HOST"
    start_redis
    write_two_node_env "$REMOTE_HOST"
    ok "compute ready — now run: python3 bench.py --csv results.csv"
}

# ── Dispatch ──────────────────────────────────────────────────────────────────
main() {
    local cmd="${1:-}"; shift || true
    case "$cmd" in
        up)       cmd_up "$@";;
        down)     cmd_down "$@";;
        status)   cmd_status "$@";;
        backend)  cmd_backend "$@";;
        compute)  cmd_compute "$@";;
        remote)   cmd_remote "$@";;
        two-node) cmd_two_node "$@";;
        ""|help|-h|--help)
            sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//';;
        *) die "unknown command '$cmd' (try: backend | compute | up | down | status | remote | two-node | help)";;
    esac
}

main "$@"
