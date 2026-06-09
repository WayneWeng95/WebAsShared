#!/bin/bash
# Rust environment setup — local to the project
# Usage: source scripts/rust.sh

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PROJECT_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

export RUSTUP_HOME="$PROJECT_ROOT/.rustup"
export CARGO_HOME="$PROJECT_ROOT/.cargo"
export PATH="$CARGO_HOME/bin:$PATH"

# Install rustup locally if not present
if [ ! -f "$CARGO_HOME/bin/rustup" ]; then
    echo "Installing Rust locally into $CARGO_HOME ..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    rustup toolchain install nightly
    rustup target add wasm32-unknown-unknown --toolchain nightly
    # rustup target add wasm64-unknown-unknown --toolchain nightly  # disabled: wasm64 has JIT performance issues
    rustup component add rust-src --toolchain nightly
fi

echo "Rust environment set:"
echo "  RUSTUP_HOME=$RUSTUP_HOME"
echo "  CARGO_HOME=$CARGO_HOME"
rustc --version
cargo --version
