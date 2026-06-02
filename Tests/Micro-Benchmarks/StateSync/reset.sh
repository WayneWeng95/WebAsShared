#!/usr/bin/env bash
#
# reset.sh — return this node to a clean StateSync state and verify it.
#
# Stops the Redis + MinIO backends, clears runtime/data artifacts, then checks
# that no benchmark processes survive and the ports are free.  Run it on each
# node before (or after) an experiment.
#
# Usage:
#   ./reset.sh                # stop + clean (keeps downloaded binaries) + check
#   ./reset.sh --purge-bin    # also delete the downloaded minio/mc binaries
#   ./reset.sh --check-only    # don't touch anything; just report current state
#   ./reset.sh --keep-data    # stop + clean metadata but leave the data dirs

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNDIR="${RUNDIR:-$SCRIPT_DIR/.run}"
ENV_FILE="$SCRIPT_DIR/backends.env"
REDIS_DIR="${REDIS_DIR:-/tmp/statesync-redis}"
S3_DATADIR="${S3_DATADIR:-/tmp/statesync-minio-data}"            # RAM-backed
S3_DISK_DATADIR="${S3_DISK_DATADIR:-/var/tmp/statesync-minio-disk}"  # disk-backed
REDIS_PORT="${REDIS_PORT:-6379}"
S3_PORT="${S3_PORT:-9000}"
S3_CONSOLE_PORT="${S3_CONSOLE_PORT:-9001}"
S3_DISK_PORT="${S3_DISK_PORT:-9010}"
S3_DISK_CONSOLE_PORT="${S3_DISK_CONSOLE_PORT:-9011}"

PURGE_BIN=0
CHECK_ONLY=0
KEEP_DATA=0
for arg in "$@"; do
    case "$arg" in
        --purge-bin)  PURGE_BIN=1;;
        --check-only) CHECK_ONLY=1;;
        --keep-data)  KEEP_DATA=1;;
        -h|--help)    sed -n '2,14p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0;;
        *) echo "unknown flag: $arg" >&2; exit 1;;
    esac
done

log()  { printf '\033[1;34m[reset]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[ ok  ]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }

# ── Clean ─────────────────────────────────────────────────────────────────────
do_clean() {
    log "stopping backends via deploy_backends.sh down"
    bash "$SCRIPT_DIR/deploy_backends.sh" down >/dev/null 2>&1 || true

    # Targeted kill of any stray instance this benchmark started (matched by our
    # own run dir / data dir, so we never touch unrelated redis/minio servers).
    pkill -f "redis-server $RUNDIR/redis.conf" 2>/dev/null || true
    pkill -f "minio server $S3_DATADIR"        2>/dev/null || true
    pkill -f "minio server $S3_DISK_DATADIR"   2>/dev/null || true
    sleep 0.3   # let sockets close before the port check

    log "clearing runtime artifacts"
    rm -f "$RUNDIR"/redis.conf "$RUNDIR"/redis.pid "$RUNDIR"/redis.log
    rm -f "$RUNDIR"/minio*.pid "$RUNDIR"/minio*.log   # minio.pid, minio-ram/disk.*
    rm -f "$ENV_FILE"

    if [[ $KEEP_DATA -eq 0 ]]; then
        # Report space reclaimed (esp. the disk-backed S3 dir on /) before removing.
        for d in "$REDIS_DIR" "$S3_DATADIR" "$S3_DISK_DATADIR"; do
            if [[ -d "$d" ]]; then
                log "removing $(du -sh "$d" 2>/dev/null | cut -f1) data dir: $d"
            fi
        done
        rm -rf "$REDIS_DIR" "$S3_DATADIR" "$S3_DISK_DATADIR"
    else
        warn "keeping data dirs (--keep-data)"
    fi

    if [[ $PURGE_BIN -eq 1 ]]; then
        log "removing downloaded binaries ($RUNDIR/bin)"
        rm -rf "$RUNDIR/bin"
    fi
    ok "clean complete"
}

# ── Check ─────────────────────────────────────────────────────────────────────
do_check() {
    local clean=1
    echo
    log "verifying clean state"

    # Processes (exact match — avoids matching this script's own command line).
    if pgrep -ax redis-server >/dev/null 2>&1; then
        warn "redis-server still running:"; pgrep -ax redis-server; clean=0
    else
        ok "no redis-server process"
    fi
    if pgrep -ax minio >/dev/null 2>&1; then
        warn "minio still running:"; pgrep -ax minio; clean=0
    else
        ok "no minio process"
    fi

    # Ports.
    for p in "$REDIS_PORT" "$S3_PORT" "$S3_CONSOLE_PORT" "$S3_DISK_PORT" "$S3_DISK_CONSOLE_PORT"; do
        if ss -ltn 2>/dev/null | grep -q ":$p "; then
            warn "port $p still LISTENING:"; ss -ltnp 2>/dev/null | grep ":$p "; clean=0
        else
            ok "port $p free"
        fi
    done

    # systemd distro Redis should not be active (deploy disables it).
    if command -v systemctl >/dev/null 2>&1; then
        local st; st="$(systemctl is-active redis-server 2>/dev/null || true)"
        if [[ "$st" == "active" ]]; then
            warn "distro redis-server service is ACTIVE (run deploy backend to stop+disable it)"; clean=0
        else
            ok "distro redis-server service: ${st:-not-present}"
        fi
    fi

    echo
    if [[ $clean -eq 1 ]]; then
        ok "node is CLEAN — ready for the experiment"
    else
        warn "node is NOT fully clean — see warnings above"
        return 1
    fi
}

# ── Main ──────────────────────────────────────────────────────────────────────
if [[ $CHECK_ONLY -eq 1 ]]; then
    do_check
else
    do_clean
    do_check
fi
