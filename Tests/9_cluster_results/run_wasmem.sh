#!/usr/bin/env bash
# Run WasMem (our framework) on the 9-node cluster with AOT sandboxes + TOTAL MEMORY count.
# Unlike the baselines (node-0 = coordinator only, 8 workers), our framework uses ALL 9 NODES
# as executors (node-0 coordinates AND runs its share). Sandboxes are AOT .cwasm (the run_*.py
# submit with --aot). Inputs RDMA-staged from node-0. total_mem_mb/mem_max_mb come from the
# node-agent's own metrics (Σ / max per-node peak rss + SHM arena) via wasmem_mem.py.
#
# NOTE: matrix@4096 and wordcount@4GB are omitted — they hit large-input bugs in the current
# build (matrix gives a fanout-dependent wrong checksum for N≥1024; wordcount's 4 GB corpus
# exceeds the SHM staging ceiling). Both are the active "sharded jobs" work item.
set -uo pipefail
ROOT=/opt/myapp/WebAsShared
WM="$ROOT/Tests/Inter-Node Application_Benchmark/wasmem"
OUT="$ROOT/Tests/9_cluster_results/wasmem"
TD="TestData"; REPS=3; NODES=9
mkdir -p "$OUT"; cd "$ROOT"
hr(){ printf '\n\033[1;32m######## %s ########\033[0m\n' "$*"; }
run(){ local name="$1"; shift; hr "WASMEM $name"; date +%T
       local csv="$OUT/wasmem_${name}.csv"; rm -f "$csv"
       local since; since=$(python3 -c 'import time;print(int(time.time()*1000))')
       if python3 "$@" --nodes "$NODES" --reps "$REPS" --csv "$csv"; then
         python3 "$WM/wasmem_mem.py" "$csv" "$since"; echo "[$name] OK"
       else echo "[$name] FAILED rc=$?"; fi; }

run finra        "$WM/run_finra.py"        --trades "$TD/finra_5m.csv"        --fanout 118
run ml_training  "$WM/run_ml_training.py"  --data   "$TD/ml_training_6m.csv"  --fanout 60
run ml_inference "$WM/run_ml_inference.py" --data   "$TD/ml_inference_6m.csv" --fanout 120
run terasort     "$WM/run_terasort.py"     --records "$TD/terasort_1.2gb.txt" --shard-input

hr "ALL WASMEM DONE"; date +%T
echo "=== CSVs ==="; for f in "$OUT"/wasmem_*.csv; do echo "--- $(basename $f) ---"; cat "$f"; done
