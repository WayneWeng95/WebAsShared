#!/usr/bin/env bash
# Run all 6 RMMap-RDMA workloads on the 9-node cluster, CSVs → 9_cluster_results.
# node-0 (10.10.1.2) is the coordinator: it publishes the RDMA buffer and is NOT a worker.
# Mappers fan out over SSH round-robin across the 8 WORKER nodes only; with fanout ≤ 128 the
# round-robin lands ≤ 16 mappers per node — the same 16/node cap Cloudburst gets from its
# topologySpread. Fanouts match the Cloudburst run so the head-to-head is apples-to-apples.
set -uo pipefail
ROOT=/opt/myapp/WebAsShared
B="$ROOT/Tests/Inter-Node deployment"
OUT="$ROOT/Tests/9_cluster_results/rmmap"
SRV=10.10.1.2; SELF=10.10.1.2; REPS=3
CAP=16; WORKER_N=8                       # 16 executors/node × 8 worker nodes = 128 slots
# 8 worker nodes ONLY — node-0 (10.10.1.2) excluded (coordinator/publisher, like Cloudburst).
NODES="10.10.1.1,10.10.1.3,10.10.1.4,10.10.1.5,10.10.1.6,10.10.1.7,10.10.1.8,10.10.1.9"
COMMON=(--server-ip "$SRV" --nodes "$NODES" --self-host "$SELF" --reps "$REPS")
mkdir -p "$OUT"; cd "$ROOT"
hr(){ printf '\n\033[1;35m######## %s ########\033[0m\n' "$*"; }
# guard: refuse a fanout that would exceed the 16/node cap over the 8 workers.
cap_ok(){ [ "$1" -le $((CAP*WORKER_N)) ] || { echo "[$2] fanout $1 > ${CAP}×${WORKER_N}=${CAP}*${WORKER_N} cap — abort"; return 1; }; }
run(){ local name="$1" dir="$2" fan="$3"; shift 3; hr "RMMAP $name"; date +%T
       cap_ok "$fan" "$name" || { echo "[$name] SKIPPED (cap)"; return; }
       python3 "$B/$dir/driver.py" "$@" "${COMMON[@]}" --csv "$OUT/rmmap_${name}.csv" \
         && echo "[$name] OK" || echo "[$name] FAILED rc=$?"; }

run wordcount    rmmap-rdma-wordcount 120 --corpus TestData/corpus_4gb.txt --fanout 120
run finra        rmmap-rdma-finra     118 --trades TestData/finra_5m.csv   --fanout 118
run ml_training  rmmap-rdma-ml        120 --mode train --data TestData/ml_training_6m.csv  --fanout 120
run ml_inference rmmap-rdma-ml        120 --mode infer --data TestData/ml_inference_6m.csv --model TestData/ml_inference_model.csv --fanout 120
run matrix       rmmap-rdma-matrix    128 --a TestData/matrix_a_4096.bin --b TestData/matrix_b_4096.bin --matrix-n 4096 --workers 128

# terasort is HOSTNAME-based (its NODE_IP map is keyed by node-N, not IPs) and needs its own
# invocation: self=node-0 (coordinator/input server), 8 WORKER hostnames (node-0 excluded).
TS_NODES="node-1,node-2,node-3,node-4,node-5,node-6,node-7,node-8"
hr "RMMAP terasort"; date +%T
if cap_ok 4 terasort; then
  python3 "$B/rmmap-rdma-terasort/driver.py" --records TestData/terasort_1.2gb.txt --fanout 4 \
    --server-ip "$SRV" --self-host node-0 --nodes "$TS_NODES" --reps "$REPS" \
    --csv "$OUT/rmmap_terasort.csv" && echo "[terasort] OK" || echo "[terasort] FAILED rc=$?"
fi

hr "ALL RMMAP DONE"; date +%T
echo "=== CSVs ==="; ls -l "$OUT"/rmmap_*.csv 2>/dev/null
