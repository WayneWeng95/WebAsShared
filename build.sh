#!/bin/bash
set -e

ROOT=$(cd "$(dirname "$0")" && pwd)

echo "=== Building Executor host ==="
cd "$ROOT/Executor"
cargo build --release

echo ""
echo "=== Building WASM guest ==="
cd "$ROOT/Executor/guest"
cargo +nightly build --release

echo ""
echo "=== Building NodeAgent ==="
cd "$ROOT/NodeAgent"
cargo build --release
cp target/release/node-agent "$ROOT/node-agent"

echo ""
echo "=== Build complete ==="
echo "  host:       $ROOT/Executor/target/release/host"
echo "  guest.wasm: $ROOT/Executor/target/wasm32-unknown-unknown/release/guest.wasm"
echo "  node-agent: $ROOT/node-agent"
