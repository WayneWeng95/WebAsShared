#!/usr/bin/env bash
# demo_auto_placement.sh
#
# Demonstrates capacity-aware auto-placement using word_count_auto.json.
#
# Three scenarios are run in sequence:
#   balanced  — both hosts equal (50/50, limit 12)  → aggregation spread
#   skewed    — host 0 idle, host 1 saturated        → everything packs on host 0
#   nohints   — no SCX data available               → uniform round-robin fallback
#
# Usage:
#   cd WebAsShared && bash demo_auto_placement.sh [balanced|skewed|nohints|all]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

PARTITION_BIN="Partitioner/target/release/partition"
HOST_BIN="Executor/target/release/host"
WASM_PATH="Executor/target/wasm32-unknown-unknown/release/guest.wasm"
AUTO_DAG="DAGs/symbolic_dag/word_count_auto.json"
LOG_DIR="/tmp/auto_placement_logs"
LOCALHOST="127.0.0.1"

GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

log()     { echo "[$(date '+%H:%M:%S')] $*"; }
section() { echo -e "\n${CYAN}══════════════════════════════════════════════════${NC}";
            echo -e "${CYAN}  $*${NC}";
            echo -e "${CYAN}══════════════════════════════════════════════════${NC}"; }
ok()      { echo -e "[$(date '+%H:%M:%S')] ${GREEN}✓  $*${NC}"; }

require_binaries() {
    [[ -x "$PARTITION_BIN" ]] || { echo "Build first: cd Partitioner && cargo build --release"; exit 1; }
    [[ -x "$HOST_BIN" ]]      || { echo "Build first: cd Executor && cargo build --release";   exit 1; }
    [[ -f "$WASM_PATH" ]]     || { echo "Build first: cd Executor/guest && cargo +nightly build --release"; exit 1; }
}

# ── Partition + split ─────────────────────────────────────────────────────────
# Writes per-node DAG JSON to temp files; prints their paths (one per line).
split_dag() {
    local hints_file="${1:-}"   # empty = no hints
    local scenario="${2:-demo}"
    local cluster_tmp; cluster_tmp="$(mktemp /tmp/cluster_XXXXXX.json)"

    if [[ -n "$hints_file" ]]; then
        "$PARTITION_BIN" "$AUTO_DAG" --hints "$hints_file" > "$cluster_tmp"
    else
        "$PARTITION_BIN" "$AUTO_DAG" > "$cluster_tmp"
    fi

    # Show placement summary (stderr so it doesn't mix with file paths on stdout).
    python3 - "$cluster_tmp" >&2 <<'PYEOF'
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
for host in sorted(d['node_dags'].keys(), key=int):
    nodes = d['node_dags'][host]
    original = [n['id'] for n in nodes
                if not n['id'].startswith('rs_') and not n['id'].startswith('rr_')]
    infra    = sum(1 for n in nodes
                   if n['id'].startswith('rs_') or n['id'].startswith('rr_'))
    auto     = [n for n in original
                if not any(n.startswith(p) for p in ['load_','distribute_','map_'])]
    print(f"  host {host}: {len(original)} original, {infra} infra  |  auto-placed: {auto}")
PYEOF

    # Split into per-node temp files and print their paths.
    python3 - "$cluster_tmp" "$scenario" "$WASM_PATH" "$LOCALHOST" <<'PYEOF'
import json, sys, tempfile

cluster_file, scenario, wasm_path, localhost = sys.argv[1:]
with open(cluster_file) as f:
    cluster = json.load(f)

shm_prefix = cluster.get('shm_path_prefix', '/dev/shm/auto_wc')
log_level  = cluster.get('log_level', 'info')
transfer   = cluster.get('transfer', False)
total      = max(int(k) for k in cluster['node_dags']) + 1

for host_str, nodes in sorted(cluster['node_dags'].items(), key=lambda x: int(x[0])):
    node_id = int(host_str)
    per_node = {
        'shm_path':  f'{shm_prefix}_n{node_id}',
        'wasm_path': wasm_path,
        'log_level': log_level,
    }
    has_remote = any('Remote' in json.dumps(n.get('kind', '')) for n in nodes)
    if transfer or has_remote:
        per_node['rdma'] = {
            'node_id':  node_id,
            'total':    total,
            'ips':      [localhost] * total,
            'transfer': transfer,
        }
    per_node['nodes'] = nodes

    tmp = tempfile.NamedTemporaryFile(
        mode='w', suffix='.json',
        prefix=f'auto_{scenario}_n{node_id}_',
        delete=False)
    json.dump(per_node, tmp, indent=2)
    tmp.close()
    print(tmp.name)
PYEOF

    rm -f "$cluster_tmp"
}

# ── Run a node pair ───────────────────────────────────────────────────────────
run_pair() {
    local name="$1" json0="$2" json1="$3" out_file="${4:-}"

    mkdir -p "$LOG_DIR"
    local merged="$LOG_DIR/${name}_merged.log"
    : > "$merged"

    (cd "$ROOT" && "$HOST_BIN" dag "$json0" 2>&1 |
        while IFS= read -r line; do printf '[%s][node0] %s\n' "$(date '+%H:%M:%S.%3N')" "$line"; done \
        >> "$merged") &
    local pid0=$!

    (cd "$ROOT" && "$HOST_BIN" dag "$json1" 2>&1 |
        while IFS= read -r line; do printf '[%s][node1] %s\n' "$(date '+%H:%M:%S.%3N')" "$line"; done \
        >> "$merged") &
    local pid1=$!

    local rc0=0 rc1=0
    wait "$pid0" || rc0=$?
    wait "$pid1" || rc1=$?

    rm -f "$json0" "$json1"

    echo "--- execution log (sorted by time) ---"
    sort "$merged"
    echo "---"

    [[ $rc0 -eq 0 && $rc1 -eq 0 ]] || { log "FAILED (rc0=$rc0 rc1=$rc1)"; return 1; }
    if [[ -n "$out_file" && ! -f "$out_file" ]]; then
        log "Expected output not found: $out_file"; return 1
    fi
}

print_results() {
    local file="$1" label="$2"
    [[ -f "$file" ]] || return 0
    log "=== $label (top 20) ==="
    sort -t= -k2 -rn "$file" | head -20
}

# ── Scenarios ─────────────────────────────────────────────────────────────────

run_balanced() {
    section "Scenario: BALANCED HOSTS (50/50 capacity, limit 12 each)"
    log "Both nodes equally loaded → dep-affinity splits aggregation across hosts"
    local hints; hints="$(mktemp /tmp/hints_XXXXXX.json)"
    printf '{"capacity":{"0":0.5,"1":0.5},"host_limit":{"0":12,"1":12}}' > "$hints"
    log "Placement:"
    local files=()
    while IFS= read -r line; do files+=("$line"); done < <(split_dag "$hints" "balanced")
    rm -f "$hints"
    log "Running DAG (loopback RDMA)..."
    run_pair "balanced" "${files[0]}" "${files[1]}" "/tmp/rdma_word_count_auto_result.txt"
    print_results "/tmp/rdma_word_count_auto_result.txt" "Balanced word count"
    ok "Balanced scenario passed"
}

run_skewed() {
    section "Scenario: SKEWED HOSTS (host 0 idle 85%, host 1 saturated limit=1)"
    log "Single-host packing → all 5 auto nodes land on host 0"
    local hints; hints="$(mktemp /tmp/hints_XXXXXX.json)"
    printf '{"capacity":{"0":0.85,"1":0.15},"host_limit":{"0":12,"1":1}}' > "$hints"
    log "Placement:"
    local files=()
    while IFS= read -r line; do files+=("$line"); done < <(split_dag "$hints" "skewed")
    rm -f "$hints"
    log "Running DAG (loopback RDMA)..."
    run_pair "skewed" "${files[0]}" "${files[1]}" "/tmp/rdma_word_count_auto_result.txt"
    print_results "/tmp/rdma_word_count_auto_result.txt" "Skewed word count"
    ok "Skewed scenario passed"
}

run_nohints() {
    section "Scenario: NO HINTS (no sched_ext data — uniform fallback)"
    log "Unlimited round-robin: auto nodes distributed evenly"
    log "Placement:"
    local files=()
    while IFS= read -r line; do files+=("$line"); done < <(split_dag "" "nohints")
    log "Running DAG (loopback RDMA)..."
    run_pair "nohints" "${files[0]}" "${files[1]}" "/tmp/rdma_word_count_auto_result.txt"
    print_results "/tmp/rdma_word_count_auto_result.txt" "No-hints word count"
    ok "No-hints scenario passed"
}

# ── main ──────────────────────────────────────────────────────────────────────

mkdir -p "$LOG_DIR"
require_binaries

SUITE="${1:-all}"
case "$SUITE" in
    balanced) run_balanced ;;
    skewed)   run_skewed   ;;
    nohints)  run_nohints  ;;
    all)
        run_balanced
        run_skewed
        run_nohints
        ;;
    *)
        echo "Usage: $0 [balanced|skewed|nohints|all]"
        exit 1
        ;;
esac

log "Logs saved in: $LOG_DIR"
