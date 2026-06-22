#!/usr/bin/env bash
# run-cluster.sh — drive the streaming workload on the cluster via `node-agent submit`.
#
# First WasMem cluster version (data-parallel + RDMA merge): generates the event
# stream, splits it round-robin into one disjoint slice per node, generates the
# ClusterDag (gen_cluster_dag.py), and submits it from the coordinator. The merger
# node (last) writes the correctness gate (total_events / login_ok / ...).
#
# PREREQS (you / the cluster prep):
#   1. Workers up:   on each worker node run `./node-agent start --config NodeAgent/agent_worker.toml`
#                    coordinator config = NodeAgent/agent_coordinator.toml (node 0, RoCE IPs 10.10.1.x).
#   2. RDMA ready:   `sudo modprobe rdma_ucm` on every node (see memory host-rdma-roce).
#   3. Inputs staged: the per-node slices written below must exist at the SAME relative
#                    path on every node. If the repo FS is shared across nodes, this
#                    script writing them once is enough; otherwise copy
#                    TestData/stream_app/cluster/events_node*.csv to each node.
#   No build needed — reuses the intra-node guest (mr_*/sn_*). (If you do rebuild, use ./build.sh.)
#
# Usage:
#   NODES=2 ./run-cluster.sh mediareview 100000
#   NODES=4 PARTS=4 USERS=10000 ./run-cluster.sh socialnetwork 200000
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUITE="$(cd "$HERE/.." && pwd)"            # …/Streaming Application_Benchmark
ROOT="$(cd "$HERE/../../.." && pwd)"        # repo root

WORKLOAD="${1:-mediareview}"
EVENTS="${2:-100000}"
NODES="${NODES:-2}"
PARTS="${PARTS:-4}"
USERS="${USERS:-10000}"
SKEW="${SKEW:-0.0}"
CFG="${CFG:-$ROOT/NodeAgent/agent_coordinator.toml}"
DATADIR_REL="TestData/stream_app/cluster"
DATADIR="$ROOT/$DATADIR_REL"
DAG="$HERE/.cluster_${WORKLOAD}.json"
OUT_REL="TestOutput/rdma_stream_${WORKLOAD}.txt"

case "$WORKLOAD" in mediareview|socialnetwork) ;; *) echo "unknown workload: $WORKLOAD" >&2; exit 1 ;; esac
log() { printf '\033[1;36m[stream-cluster]\033[0m %s\n' "$*"; }
[ -x "$ROOT/node-agent" ] || { echo "node-agent not built (./build.sh)"; exit 1; }
mkdir -p "$DATADIR" "$ROOT/TestOutput"

# 1. generate the full deterministic stream, split round-robin into N disjoint slices
log "gen_events: $WORKLOAD events=$EVENTS users=$USERS skew=$SKEW  → $NODES slices"
python3 "$SUITE/intra-node/gen_events.py" "$WORKLOAD" --events "$EVENTS" --users "$USERS" --skew "$SKEW" > /tmp/.cl_events.csv
python3 - "$NODES" "$DATADIR" <<'PY'
import sys
nodes, ddir = int(sys.argv[1]), sys.argv[2]
lines = [l for l in open('/tmp/.cl_events.csv') if l.strip() and not l.startswith('#')]
outs = [open(f"{ddir}/events_node{i}.csv", "w") for i in range(nodes)]
for idx, l in enumerate(lines):
    outs[idx % nodes].write(l)          # round-robin → each event on exactly one node
for o in outs:
    o.close()
print(f"split {len(lines)} events into {nodes} disjoint slices in {ddir}")
PY

# 2. generate the ClusterDag (relative input/output paths → resolve per-node under executor_work_dir)
log "gen_cluster_dag → $DAG"
python3 "$HERE/gen_cluster_dag.py" "$WORKLOAD" --nodes "$NODES" --parts "$PARTS" --users "$USERS" \
    --events-prefix "$DATADIR_REL/events_node" --out "$OUT_REL" --persist "${PERSIST:-none}" > "$DAG"

# 3. submit from the coordinator
log "submit: node-agent submit --config $CFG --dag $DAG"
( cd "$ROOT" && ./node-agent submit --config "$CFG" --dag "$DAG" ); rc=$?

echo
log "submit exit=$rc"
log "correctness gate (on the MERGER = node $((NODES-1))): $ROOT/$OUT_REL"
log "expected: total_events == $EVENTS ; login_ok == #login events in the stream"
exit "$rc"
