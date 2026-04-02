#!/bin/bash
# Rust environment setup for /opt installation
# Usage: source /opt/start.sh

export RUSTUP_HOME="/opt/rustup"
export CARGO_HOME="/opt/cargo"
export PATH="/opt/cargo/bin:$PATH"

# Re-add to ~/.bashrc and ~/.profile if missing (home dir may have been reset)
for rc in "$HOME/.bashrc" "$HOME/.profile"; do
    if [ -f "$rc" ] && ! grep -q "/opt/cargo/env" "$rc"; then
        echo ". /opt/cargo/env" >> "$rc"
        echo "Restored Rust env hook in $rc"
    fi
done

echo "Rust environment set:"
echo "  RUSTUP_HOME=$RUSTUP_HOME"
echo "  CARGO_HOME=$CARGO_HOME"
rustc --version
cargo --version
