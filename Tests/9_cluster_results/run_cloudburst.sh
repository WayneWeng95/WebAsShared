#!/usr/bin/env bash
# Run all 6 Cloudburst workloads on the 9-node cluster: 3 COLD + 12 WARM reps each (15 total).
# The drivers self-manage the executor pool via exec_pool.py — cold reps launch executors ON the
# job's wave (launch folded into the cold makespan), warm reps reuse the pool; each driver leaves
# the pool at 0 on exit. No pre-scale needed here. CSVs → 9_cluster_results.
set -uo pipefail
ROOT=/opt/myapp/WebAsShared
CB="$ROOT/Tests/Inter-Node deployment/k8s/cloudburst"
OUT="$ROOT/Tests/9_cluster_results/cloudburst"
RH=10.10.1.2; RP=30679; WARM=12; COLD=3
mkdir -p "$OUT"
cd "$ROOT"
hr(){ printf '\n\033[1;34m######## %s ########\033[0m\n' "$*"; }
run(){ local name="$1"; shift; hr "CLOUDBURST $name"; date +%T
       python3 "$CB/$1" "${@:2}" --warm-reps "$WARM" --cold-reps "$COLD" \
         --redis-host "$RH" --redis-port "$RP" \
         --csv "$OUT/cloudburst_${name}.csv" && echo "[$name] OK" || echo "[$name] FAILED rc=$?"; }

# Fanouts scaled to fill the 8-worker × 16-executor (128-slot) 9-node capacity.
# matrix must factor cleanly over N=4096 → 8×16=128 is the only clean grid >64.
# terasort stays at 4: its all-to-all is N² Redis buckets (4→16) and a ~1.3 GB/task
# memory design point; 120 owners would blow up to 14400 keys/rep with no fair RMMap
# counterpart — kept at the wasmem/faasm-matched 4 range owners.
run wordcount    driver.py            --corpus TestData/corpus_4gb.txt   --fanout 120
run finra        driver_finra.py      --trades TestData/finra_5m.csv     --fanout 118
run ml_training  driver_ml_training.py --data  TestData/ml_training_6m.csv  --workers 120
run ml_inference driver_ml_inference.py --data TestData/ml_inference_6m.csv --model TestData/ml_inference_model.csv --workers 120
run matrix       driver_matrix.py     --a TestData/matrix_a_4096.bin --b TestData/matrix_b_4096.bin --matrix-n 4096 --workers 128
run terasort     driver_terasort.py   --records TestData/terasort_1.2gb.txt --fanout 4

hr "ALL CLOUDBURST DONE"; date +%T
echo "=== CSVs ==="; ls -l "$OUT"/cloudburst_*.csv 2>/dev/null
