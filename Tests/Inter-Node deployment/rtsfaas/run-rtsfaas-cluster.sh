#!/usr/bin/env bash
# run-rtsfaas-cluster.sh — multi-node RTSFaaS test (track E of the inter-node
# deployment plan). Distributes the 4 RTSFaaS roles across the cluster over RoCE:
#   node0 (coordinator)  : database + driver + client
#   worker nodes         : one `worker` role each
# Uses the in-memory RemoteStorageManager (isRemoteDB=1, isTiKV=0) — NO TiKV for
# this first test — and isRDMA=1 across real machines. Reuses the built rtfaas:1.0
# image and the single-node DockerFiles scripts (small heap + the DB-role join patch
# already in the jar). See README.md for the one-time PREREQS before running this.
#
# Usage:
#   ./run-rtsfaas-cluster.sh MediaReview              # 1 worker (node1), default
#   WORKERS="10.10.1.1 10.10.1.4" ./run-rtsfaas-cluster.sh SocialNetwork   # 2 workers
#   KEEP=1 ./run-rtsfaas-cluster.sh MediaReview       # leave containers up
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SINGLE="$(cd "$HERE/../../Streaming Application_Benchmark/intra-node/baseline/RTSFaaS/single_node" && pwd)"
DOCKER="${DOCKER:-sudo docker}"
SSH="ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=8"

APP="${1:-MediaReview}"
# RoCE IP -> reverse-DNS name (RTSFaaS matches workers by getHostName()).
declare -A NAME=( [10.10.1.2]=node-0-link-0 [10.10.1.1]=node-1-link-0 \
                  [10.10.1.4]=node-2-link-0 [10.10.1.3]=node-3-link-0 )
COORD_IP="${COORD_IP:-10.10.1.2}"            # node0: database+driver+client
WORKERS="${WORKERS:-10.10.1.1}"               # space-separated worker IPs
RTFAAS_DIR="${RTFAAS_DIR:-/opt/myapp/compare_system/RTSFaaS}"  # repo path (same on all nodes)
log() { printf '\033[1;36m[rtsfaas-cluster]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[rtsfaas-cluster] %s\033[0m\n' "$*" >&2; exit 1; }

# ---- build worker host/port lists from WORKERS ----
wnames=(); wports=(); i=0
for ip in $WORKERS; do
  [ -n "${NAME[$ip]:-}" ] || die "unknown worker IP $ip (add it to NAME[])"
  wnames+=("${NAME[$ip]}"); wports+=("$((5550 + i*10))"); i=$((i+1))
done
WORKER_NUM=${#wnames[@]}
WORKER_HOSTS=$(IFS=,; echo "${wnames[*]}")
WORKER_PORTS=$(IFS=,; echo "${wports[*]}")
COORD_NAME="${NAME[$COORD_IP]}"

# ---- 1. assemble a FaaS dir (Env + DockerFiles) for distribution ----
WORK="$HERE/.faas_cluster"; rm -rf "$WORK"; mkdir -p "$WORK/Env" "$WORK/DockerFiles"
cp "$SINGLE"/DockerFiles/*.sh "$WORK/DockerFiles/"; chmod +x "$WORK"/DockerFiles/*.sh
cp "$SINGLE"/Env/System.env "$SINGLE"/Env/MediaReview.env "$SINGLE"/Env/SocialNetwork.env "$WORK/Env/" 2>/dev/null
cat > "$WORK/Env/Cluster.env" <<E
gatewayHost=localhost
gatewayPort=5556
driverHost=$COORD_NAME
driverPort=5559
workerNum=$WORKER_NUM
workerHosts=$WORKER_HOSTS
workerPorts=$WORKER_PORTS
E
cat > "$WORK/Env/Database.env" <<E
isRemoteDB=1
isDynamoDB=0
isTiKV=0
r_w_capacity_unit=10
databaseHost=$COORD_NAME
databasePort=2381
E
log "topology: coordinator=$COORD_NAME (db+driver+client); workers=$WORKER_HOSTS (num=$WORKER_NUM)"

# ---- 1b. SETUP=1: install deps + build jar/image on every node (idempotent) ----
# The repo is git-synced (source only), so each node must build the jar/image and
# install Java/Maven + rdma_ucm. setup-node.sh does this; it's a no-op where done.
WAS_DIR="${WAS_DIR:-/opt/myapp/WebAsShared}"
SETUP_SH="$WAS_DIR/Tests/Inter-Node deployment/rtsfaas/setup-node.sh"
if [ "${SETUP:-0}" = 1 ]; then
  log "SETUP coordinator ($COORD_IP) ..."; bash "$SETUP_SH" || die "setup failed on coordinator"
  for ip in $WORKERS; do
    log "SETUP worker $ip ... (first build downloads maven deps, ~minutes)"
    $SSH "$ip" "bash \"$SETUP_SH\"" || die "setup failed on $ip (ssh ok? see README PREREQS)"
  done
fi

# ---- 2. distribute FaaS dir to each worker node (image present via SETUP or prereq) ----
for ip in $WORKERS; do
  log "staging FaaS dir → $ip"
  $SSH "$ip" "mkdir -p $RTFAAS_DIR/scripts/FaaS_cluster" 2>/dev/null \
    || die "ssh to $ip failed — set up passwordless ssh (see README PREREQS)"
  scp -q -o StrictHostKeyChecking=no -r "$WORK"/. "$ip:$RTFAAS_DIR/scripts/FaaS_cluster/" \
    || die "scp to $ip failed"
done

# ---- role launcher ----
ENVS="--env-file Env/Cluster.env --env-file Env/System.env --env-file Env/$APP.env --env-file Env/Database.env"
mounts() { echo "-v $RTFAAS_DIR/scripts/FaaS_cluster/DockerFiles/$1:/rtfaas/$1 -v $RTFAAS_DIR/scripts/FaaS_cluster/DockerFiles/entrypoint.sh:/rtfaas/entrypoint.sh"; }
DEV="--privileged --device=/dev/infiniband/"
local_run() { ( cd "$WORK" && $DOCKER run -d --name "$1" --network host $ENVS $(mounts "$2") $DEV rtfaas:1.0 "${@:3}" ); }
remote_run() { local ip="$1"; shift; $SSH "$ip" "cd $RTFAAS_DIR/scripts/FaaS_cluster && sudo docker run -d --name $1 --network host $ENVS $(mounts "$2") $DEV rtfaas:1.0 ${*:3}"; }

$DOCKER rm -f rt-database rt-driver rt-client >/dev/null 2>&1
for ip in $WORKERS; do $SSH "$ip" "sudo docker rm -f rt-worker >/dev/null 2>&1" 2>/dev/null; done

# ---- 3. launch: database → driver → workers → client ----
log "start database (node0)"; local_run rt-database database.sh database >/dev/null; sleep 8
log "start driver (node0)";   local_run rt-driver   driver.sh   driver   >/dev/null; sleep 12
wid=0
for ip in $WORKERS; do
  log "start worker $wid → $ip"; remote_run "$ip" rt-worker worker.sh worker "$wid" >/dev/null; wid=$((wid+1)); sleep 8
done
log "start client (node0) — runs to completion"
( cd "$WORK" && $DOCKER run --rm --name rt-client --network host $ENVS $(mounts client.sh) $DEV rtfaas:1.0 client 2>&1 ) \
  | grep -aE "Throughput:|Latency|Exception|error" | grep -vaiE "PrintGC"

# ---- 4. teardown ----
if [ "${KEEP:-0}" != 1 ]; then
  log "teardown"
  $DOCKER rm -f rt-database rt-driver >/dev/null 2>&1
  for ip in $WORKERS; do $SSH "$ip" "sudo docker rm -f rt-worker >/dev/null 2>&1" 2>/dev/null; done
fi
log "done (app=$APP, workers=$WORKER_NUM)"
