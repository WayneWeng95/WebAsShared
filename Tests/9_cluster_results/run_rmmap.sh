#!/usr/bin/env bash
# Run all 6 RMMap-RDMA workloads on the 9-node cluster, reps=3, CSVs → 9_cluster_results.
# Mappers fan out over SSH (IPs); node-0 (10.10.1.2) publishes the RDMA buffer.
set -uo pipefail
ROOT=/opt/myapp/WebAsShared
B="$ROOT/Tests/Inter-Node deployment"
OUT="$ROOT/Tests/9_cluster_results"
SRV=10.10.1.2; SELF=10.10.1.2; REPS=3
NODES="10.10.1.2,10.10.1.1,10.10.1.3,10.10.1.4,10.10.1.5,10.10.1.6,10.10.1.7,10.10.1.8,10.10.1.9"
COMMON=(--server-ip "$SRV" --nodes "$NODES" --self-host "$SELF" --reps "$REPS")
mkdir -p "$OUT"; cd "$ROOT"
hr(){ printf '\n\033[1;35m######## %s ########\033[0m\n' "$*"; }
run(){ local name="$1" dir="$2"; shift 2; hr "RMMAP $name"; date +%T
       python3 "$B/$dir/driver.py" "$@" "${COMMON[@]}" --csv "$OUT/rmmap_${name}.csv" \
         && echo "[$name] OK" || echo "[$name] FAILED rc=$?"; }

run wordcount    rmmap-rdma-wordcount --corpus TestData/corpus_4gb.txt --fanout 60
run finra        rmmap-rdma-finra     --trades TestData/finra_5m.csv   --fanout 60
run ml_training  rmmap-rdma-ml        --mode train --data TestData/ml_training_6m.csv  --fanout 60
run ml_inference rmmap-rdma-ml        --mode infer --data TestData/ml_inference_6m.csv --model TestData/ml_inference_model.csv --fanout 60
run matrix       rmmap-rdma-matrix    --a TestData/matrix_a_4096.bin --b TestData/matrix_b_4096.bin --matrix-n 4096 --workers 64
run terasort     rmmap-rdma-terasort  --records TestData/terasort_1.2gb.txt --fanout 4

hr "ALL RMMAP DONE"; date +%T
echo "=== CSVs ==="; ls -l "$OUT"/rmmap_*.csv 2>/dev/null
