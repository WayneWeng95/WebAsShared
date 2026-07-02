#!/usr/bin/env bash
# Run all 6 Faasm workloads on the 9-node cluster with the 16/node limitation, CSVs → faasm/.
# node-0 is the coordinator (runs the reduce Faaslet, stages input into Redis) and is NOT a
# worker — map/compute Faaslets place over the 8 workers only (fl.map_nodes), ≤16/node.
# Inputs + fanouts match the Cloudburst/RMMap runs. Agents must be up on all 9 nodes
# (deployment/faasm/deploy.sh / the provisioning that installed wasmtime v45 + redis-py).
set -uo pipefail
ROOT=/opt/myapp/WebAsShared
BM="$ROOT/Tests/Inter-Node Application_Benchmark/faasm"
OUT="$ROOT/Tests/9_cluster_results/faasm"
TD="$ROOT/TestData"
REPS=3; NODES=8                       # 8 worker nodes (node-0 excluded); ≤16 faaslets/node
mkdir -p "$OUT"; cd "$BM"
hr(){ printf '\n\033[1;33m######## %s ########\033[0m\n' "$*"; }
run(){ local name="$1"; shift; hr "FAASM $name"; date +%T
       python3 "$@" --nodes "$NODES" --reps "$REPS" --csv "$OUT/faasm_${name}.csv" \
         && echo "[$name] OK" || echo "[$name] FAILED rc=$?"; }

run wordcount    run_wordcount.py    --corpus "$TD/corpus_4gb.txt"        --mappers 120
run finra        run_finra.py        --trades "$TD/finra_5m.csv"          --fanout 118
run ml_training  run_ml_training.py  --data   "$TD/ml_training_6m.csv"    --workers 120
run ml_inference run_ml_inference.py --data   "$TD/ml_inference_6m.csv"   --model "$TD/ml_inference_model.csv" --workers 120
run matrix       run_matrix.py       --matrix-n 4096                       --workers 128
run terasort     run_terasort.py     --records "$TD/terasort_1.2gb.txt"   --workers 4

hr "ALL FAASM DONE"; date +%T
echo "=== CSVs ==="; ls -l "$OUT"/faasm_*.csv 2>/dev/null
