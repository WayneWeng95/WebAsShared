#!/usr/bin/env bash
# test_rdma_single_machine.sh — run all RDMA demo pairs on a single machine.
#
# Both "nodes" use 127.0.0.1 and are launched in parallel because the RDMA
# mesh TCP bootstrap requires both sides to be ready at roughly the same time.
# Node 0 and node 1 bind/connect to each other's loopback ports.
#
# Usage:
#   cd Executor
#   bash test_rdma_single_machine.sh [wasm|py|img|all]
#
#   wasm  — WASM word count pair
#   py    — Python word count pair
#   img   — WASM image pipeline pair
#   pyimg — Python image pipeline pair
#   all   — run all four pairs in sequence (default)
#
# Prerequisites:
#   cargo build --release                        (builds ./target/release/host)
#   cargo build --release -p guest               (builds guest.wasm if needed)
#   data/corpus.txt                              (word count input)
#   data/img_gradient.ppm data/img_checkerboard.ppm data/img_rings.ppm

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# The host binary resolves relative paths (../target/..., ../data/...) from
# the host/ subdirectory — run it from there so all default paths line up.
HOST_BIN="$SCRIPT_DIR/target/release/host"   # absolute path
HOST_DIR="$SCRIPT_DIR/host"                  # working directory for host invocations
DEMO_DIR="$SCRIPT_DIR/../DAGs/rdma_demo_dag"
LOG_DIR="/tmp/rdma_test_logs"
LOCALHOST="127.0.0.1"

# ── helpers ──────────────────────────────────────────────────────────────────

log()  { echo "[$(date '+%H:%M:%S')] $*"; }
ok()   { echo "[$(date '+%H:%M:%S')] ✓  $*"; }
fail() { echo "[$(date '+%H:%M:%S')] ✗  $*" >&2; exit 1; }

require_binary() {
    [[ -x "$HOST_BIN" ]] || fail "host binary not found at $HOST_BIN — run: cd Executor && cargo build --release"
    [[ -d "$HOST_DIR" ]] || fail "host/ directory not found at $HOST_DIR"
}

# Patch 'ips' in a JSON demo file to use 127.0.0.1 for both entries,
# writing the result to a temp file.  Prints the temp file path.
patch_ips() {
    local src="$1"
    local tmp
    tmp="$(mktemp /tmp/rdma_demo_XXXXXX.json)"
    # Replace the ips array values with 127.0.0.1.
    # Works for exactly 2-node configs (the only kind in the demo files).
    sed 's/"ips": \["[^"]*", "[^"]*"\]/"ips": ["'"$LOCALHOST"'", "'"$LOCALHOST"'"]/g' \
        "$src" > "$tmp"
    echo "$tmp"
}

# Run a node0/node1 pair in parallel, wait for both, report result.
run_pair() {
    local name="$1"
    local json0="$2"
    local json1="$3"
    local out_check="${4:-}"   # optional path to verify was created

    log "=== $name ==="

    mkdir -p "$LOG_DIR"
    local log0="$LOG_DIR/${name}_node0.log"
    local log1="$LOG_DIR/${name}_node1.log"

    local tmp0 tmp1
    tmp0="$(patch_ips "$json0")"
    tmp1="$(patch_ips "$json1")"

    local merged_log="$LOG_DIR/${name}_merged.log"

    # Launch both nodes concurrently, tagging each output line with a wall-clock
    # timestamp and node label.  Both streams write to a shared merged log so the
    # interleaving of send/recv across nodes is directly visible.
    (cd "$HOST_DIR" && "$HOST_BIN" dag "$tmp0" 2>&1 | \
        while IFS= read -r line; do printf '[%s][node0] %s\n' "$(date '+%H:%M:%S.%3N')" "$line"; done | \
        tee -a "$log0" >> "$merged_log") &
    local pid0=$!
    (cd "$HOST_DIR" && "$HOST_BIN" dag "$tmp1" 2>&1 | \
        while IFS= read -r line; do printf '[%s][node1] %s\n' "$(date '+%H:%M:%S.%3N')" "$line"; done | \
        tee -a "$log1" >> "$merged_log") &
    local pid1=$!

    local rc0=0 rc1=0
    wait "$pid0" || rc0=$?
    wait "$pid1" || rc1=$?

    rm -f "$tmp0" "$tmp1"

    # Print the merged interleaved log (sorted by timestamp) so the concurrent
    # send↔recv handshake is plainly visible in sequence.
    echo "--- merged log (sorted by time) ---"
    sort "$merged_log"
    echo "---"

    if [[ $rc0 -ne 0 ]]; then log "Node 0 FAILED (rc=$rc0)"; fi
    if [[ $rc1 -ne 0 ]]; then log "Node 1 FAILED (rc=$rc1)"; fi
    if [[ $rc0 -ne 0 || $rc1 -ne 0 ]]; then
        fail "$name FAILED"
    fi

    if [[ -n "$out_check" && ! -f "$out_check" ]]; then
        fail "$name: expected output not found: $out_check"
    fi

    ok "$name passed"
}

# Tee all output to a result file as well as stdout.
RESULT_FILE="$LOG_DIR/result.txt"
exec > >(tee -a "$RESULT_FILE") 2>&1

# ── individual test cases ─────────────────────────────────────────────────────

print_word_results() {
    local file="$1" label="$2"
    if [[ -f "$file" ]]; then
        log "=== $label ==="
        sort -t= -k2 -rn "$file"
        log "=== end of $label ==="
    fi
}

test_wasm_word_count() {
    run_pair "rdma_wasm_word_count" \
        "$DEMO_DIR/rdma_word_count_node0.json" \
        "$DEMO_DIR/rdma_word_count_node1.json" \
        "/tmp/rdma_word_count_result.txt"

    print_word_results /tmp/rdma_word_count_result.txt "WASM word count results"
}

test_py_word_count() {
    run_pair "rdma_py_word_count" \
        "$DEMO_DIR/rdma_py_word_count_node0.json" \
        "$DEMO_DIR/rdma_py_word_count_node1.json" \
        "/tmp/rdma_py_word_count_result.txt"

    print_word_results /tmp/rdma_py_word_count_result.txt "Python word count results"
}

test_wasm_img_pipeline() {
    mkdir -p /tmp/rdma_img_pipeline_out

    # All 3 processed images are written as records into a single output file
    # because the RDMA pipeline completes all 3 rounds in one DAG run.
    run_pair "rdma_wasm_img_pipeline" \
        "$DEMO_DIR/rdma_img_pipeline_node0.json" \
        "$DEMO_DIR/rdma_img_pipeline_node1.json" \
        "/tmp/rdma_img_pipeline_out/gradient_processed.pgm"

    log "=== WASM image pipeline output files ==="
    ls -lh /tmp/rdma_img_pipeline_out/
    log "=== end of WASM image pipeline output files ==="
}

test_py_img_pipeline() {
    mkdir -p /tmp/rdma_py_img_pipeline_out

    run_pair "rdma_py_img_pipeline" \
        "$DEMO_DIR/rdma_py_img_pipeline_node0.json" \
        "$DEMO_DIR/rdma_py_img_pipeline_node1.json" \
        "/tmp/rdma_py_img_pipeline_out/gradient_processed.pgm"

    log "=== Python image pipeline output files ==="
    ls -lh /tmp/rdma_py_img_pipeline_out/
    log "=== end of Python image pipeline output files ==="
}

# ── main ─────────────────────────────────────────────────────────────────────

mkdir -p "$LOG_DIR"
: > "$RESULT_FILE"   # truncate on each run

require_binary

SUITE="${1:-all}"

case "$SUITE" in
    wasm)  test_wasm_word_count ;;
    py)    test_py_word_count ;;
    img)   test_wasm_img_pipeline ;;
    pyimg) test_py_img_pipeline ;;
    all)
        test_wasm_word_count
        test_py_word_count
        test_wasm_img_pipeline
        test_py_img_pipeline
        ;;
    *)
        echo "Usage: $0 [wasm|py|img|pyimg|all]"
        exit 1
        ;;
esac

log "All requested tests passed."
log "Full output saved to: $RESULT_FILE"
