#!/bin/bash
set -e
cd "$(dirname "$0")"
cargo build --release
cp target/release/node-agent ../node-agent
echo "node-agent built and copied to project root"
