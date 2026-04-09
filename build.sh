#!/bin/bash
set -e

ROOT=$(cd "$(dirname "$0")" && pwd)

# ── WASM64 mode (disabled — wasm64 compiles but has JIT performance issues) ──
# Set WASM64=1 to build for wasm64 (memory64) instead of wasm32.
#   WASM64=1 ./build.sh
#
# Default: wasm32 (unchanged behavior).

# if [ "${WASM64:-0}" = "1" ]; then
#     WASM_TARGET="wasm64-unknown-unknown"
#     FEATURES="--features wasm64"
#     MAX_MEM="17179869184"   # 16 GiB (wasm-ld max)
#     echo "*** WASM64 mode enabled ***"
# else
    WASM_TARGET="wasm32-unknown-unknown"
    FEATURES=""
    MAX_MEM="4294967296"    # 4 GiB
# fi

# ── Generate guest .cargo/config.toml for the selected target ────────────────
GUEST_CARGO_DIR="$ROOT/Executor/guest/.cargo"
mkdir -p "$GUEST_CARGO_DIR"
RUSTFLAGS_EXTRA=""
# if [ "${WASM64:-0}" = "1" ]; then
#     # wasm64: initial memory must match the host's MemoryType min (16384 pages * 64KiB = 1 GiB)
#     INIT_MEM="1073741824"
#     RUSTFLAGS_EXTRA="  \"-C\", \"link-arg=--initial-memory=$INIT_MEM\","
# fi

cat > "$GUEST_CARGO_DIR/config.toml" <<EOF
[build]
target = "$WASM_TARGET"

[unstable]
build-std = ["std", "panic_abort"]

[target.$WASM_TARGET]
rustflags = [
  "-C", "target-feature=+atomics,+bulk-memory",
  "-C", "link-arg=--import-memory",
  "-C", "link-arg=--shared-memory",
  "-C", "link-arg=--max-memory=$MAX_MEM",
$RUSTFLAGS_EXTRA  "-C", "link-arg=--allow-undefined",
]
EOF

echo "=== Building Executor host ==="
cd "$ROOT/Executor"
cargo build --release $FEATURES

echo ""
echo "=== Building WASM guest ($WASM_TARGET) ==="
cd "$ROOT/Executor/guest"
cargo +nightly build --release $FEATURES

echo ""
echo "=== Building NodeAgent ==="
cd "$ROOT/NodeAgent"
cargo build --release
cp target/release/node-agent "$ROOT/node-agent"

echo ""
echo "=== Build complete ==="
echo "  host:       $ROOT/Executor/target/release/host"
echo "  guest.wasm: $ROOT/Executor/target/$WASM_TARGET/release/guest.wasm"
echo "  node-agent: $ROOT/node-agent"
