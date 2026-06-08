#!/bin/bash
# Single-node streaming-pipeline test pass (StreamPipeline + PyPipeline).
# Builds if artifacts are missing, then runs the assertion harness.
set -e
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HOST="$ROOT/Executor/target/release/host"
if [ ! -x "$HOST" ]; then
    echo "host binary missing — building via ./build.sh ..."
    "$ROOT/build.sh"
fi
exec python3 "$ROOT/Tests/Streaming/run.py"
