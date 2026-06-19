#!/usr/bin/env bash
# run-coordinator.sh — run THIS node as the RTSFaaS coordinator (database + driver
# + client) and COLLECT the result here. The other nodes are pure workers: on each
# you separately run `./run-role.sh worker <i>` (no ssh needed — all nodes read the
# git-synced cluster.env and rendezvous over RDMA).
#
# Flow:
#   1. this node: ./run-coordinator.sh         # starts database + driver, then waits
#   2. each worker node (in WORKER_IPS order):  ./run-role.sh worker 0   (then 1, 2, ...)
#   3. once all workers connect, this script runs the client and prints Throughput.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SINGLE="$(cd "$HERE/../../Streaming Application_Benchmark/intra-node/baseline/RTSFaaS/single_node" && pwd)"
source "$HERE/cluster.env"
DOCKER="${DOCKER:-sudo docker}"
APP="${APP:-MediaReview}"
WAIT_S="${WAIT_S:-180}"   # how long to wait for all workers to connect
die() { printf '\033[1;31m[coord] %s\033[0m\n' "$*" >&2; exit 1; }
log() { printf '\033[1;36m[coord]\033[0m %s\n' "$*"; }

declare -A NAME=( [10.10.1.2]=node-0-link-0 [10.10.1.1]=node-1-link-0 \
                  [10.10.1.4]=node-2-link-0 [10.10.1.3]=node-3-link-0 )
[ -n "${NAME[$COORD_IP]:-}" ] || die "unknown COORD_IP $COORD_IP"
COORD_NAME="${NAME[$COORD_IP]}"
wnames=(); wports=(); i=0
for ip in $WORKER_IPS; do
  [ -n "${NAME[$ip]:-}" ] || die "unknown worker IP $ip"
  wnames+=("${NAME[$ip]}"); wports+=("$((5550 + i*10))"); i=$((i+1))
done
WORKER_NUM=${#wnames[@]}
WORKER_HOSTS=$(IFS=,; echo "${wnames[*]}"); WORKER_PORTS=$(IFS=,; echo "${wports[*]}")
[ "$WORKER_NUM" -ge 1 ] || die "set WORKER_IPS in cluster.env"

sudo modprobe rdma_ucm 2>/dev/null || true
[ -n "$($DOCKER images rtfaas:1.0 -q 2>/dev/null)" ] || die "rtfaas:1.0 not built — run: bash setup-node.sh"

WORK="$HERE/.coord"; rm -rf "$WORK"; mkdir -p "$WORK/Env" "$WORK/DockerFiles"
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
ENVS="--env-file Env/Cluster.env --env-file Env/System.env --env-file Env/$APP.env --env-file Env/Database.env"
mounts() { echo "-v $WORK/DockerFiles/$1:/rtfaas/$1 -v $WORK/DockerFiles/entrypoint.sh:/rtfaas/entrypoint.sh"; }
DEV="--privileged --device=/dev/infiniband/"
cd "$WORK"

$DOCKER rm -f rt-database rt-driver rt-client >/dev/null 2>&1
log "coordinator=$COORD_NAME  app=$APP  expecting $WORKER_NUM worker(s): $WORKER_HOSTS"
log "starting database + driver (this node)"
$DOCKER run -d --name rt-database --network host $ENVS $(mounts database.sh) $DEV rtfaas:1.0 database >/dev/null
sleep 8
$DOCKER run -d --name rt-driver   --network host $ENVS $(mounts driver.sh)   $DEV rtfaas:1.0 driver   >/dev/null

echo
log ">>> NOW start the workers (one per worker node), in WORKER_IPS order:"
i=0; for ip in $WORKER_IPS; do echo "      on $ip (${NAME[$ip]}):   ./run-role.sh worker $i"; i=$((i+1)); done
echo
log "waiting up to ${WAIT_S}s for all $WORKER_NUM worker(s) to connect to the driver..."
connected=0
for t in $(seq 1 $((WAIT_S/3))); do
  # the driver logs one "Driver accepts /<ip>" per worker that connects
  n=$($DOCKER logs rt-driver 2>&1 | grep -ac "Driver accepts")
  if [ "$n" -ge "$WORKER_NUM" ]; then connected=1; log "all $WORKER_NUM worker(s) connected"; break; fi
  sleep 3
done
[ "$connected" = 1 ] || { log "only $($DOCKER logs rt-driver 2>&1|grep -ac 'Driver accepts')/$WORKER_NUM connected after ${WAIT_S}s"; $DOCKER logs rt-driver 2>&1 | tail -8; }

log "running client (collecting result on this node)"
echo "======== RTSFaaS $APP — cluster ($WORKER_NUM worker(s)) ========"
$DOCKER run --rm --name rt-client --network host $ENVS $(mounts client.sh) $DEV rtfaas:1.0 client 2>&1 \
  | grep -aE "Throughput:|Latency" | grep -vaiE "PrintGC"
echo "--- driver ---"; $DOCKER logs rt-driver 2>&1 | grep -aE "finished with throughput|average latency|99th latency"

if [ "${KEEP:-0}" != 1 ]; then log "teardown (database + driver). Stop the workers with Ctrl-C on their nodes."; $DOCKER rm -f rt-database rt-driver >/dev/null 2>&1; fi
