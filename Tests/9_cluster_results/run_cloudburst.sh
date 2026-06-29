#!/usr/bin/env bash
# Run all 6 Cloudburst workloads on the 9-node cluster, reps=3, CSVs → 9_cluster_results.
set -uo pipefail
ROOT=/opt/myapp/WebAsShared
CB="$ROOT/Tests/Inter-Node deployment/k8s/cloudburst"
OUT="$ROOT/Tests/9_cluster_results"
RH=10.10.1.2; RP=30679; REPS=3
mkdir -p "$OUT"
cd "$ROOT"
hr(){ printf '\n\033[1;34m######## %s ########\033[0m\n' "$*"; }
run(){ local name="$1"; shift; hr "CLOUDBURST $name"; date +%T
       python3 "$CB/$1" "${@:2}" --reps "$REPS" --redis-host "$RH" --redis-port "$RP" \
         --csv "$OUT/cloudburst_${name}.csv" && echo "[$name] OK" || echo "[$name] FAILED rc=$?"; }

run wordcount    driver.py            --corpus TestData/corpus_4gb.txt   --fanout 60
run finra        driver_finra.py      --trades TestData/finra_5m.csv     --fanout 60
run ml_training  driver_ml_training.py --data  TestData/ml_training_6m.csv  --workers 60
run ml_inference driver_ml_inference.py --data TestData/ml_inference_6m.csv --model TestData/ml_inference_model.csv --workers 60
run matrix       driver_matrix.py     --a TestData/matrix_a_4096.bin --b TestData/matrix_b_4096.bin --matrix-n 4096 --workers 64
run terasort     driver_terasort.py   --records TestData/terasort_1.2gb.txt --fanout 4

hr "ALL CLOUDBURST DONE"; date +%T
echo "=== CSVs ==="; ls -l "$OUT"/cloudburst_*.csv 2>/dev/null
