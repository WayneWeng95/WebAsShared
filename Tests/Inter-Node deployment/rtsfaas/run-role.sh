#!/usr/bin/env bash
# run-role.sh <database|driver|worker|client> [workerId] — run ONE RTSFaaS role on
# THIS node. NO ssh: launch each role per node by hand (each node has the git-synced
# repo + the rtfaas:1.0 image from setup-node.sh). All nodes derive the SAME RTSFaaS
# Cluster.env from the git-synced cluster.env, so roles rendezvous over RDMA.
#
# Manual launch order (one command per terminal/tmux pane):
#   every node : bash setup-node.sh                 # once (installs + builds)
#   node0      : ./run-role.sh database             # leave running
#   node0      : ./run-role.sh driver               # leave running
#   worker i   : ./run-role.sh worker <i>           # i = 0,1,2... per worker node
#   node0      : ./run-role.sh client               # prints Throughput, then exits
# (database → driver → workers → client; give each ~10s before the next.)
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SINGLE="$(cd "$HERE/../../Streaming Application_Benchmark/intra-node/baseline/RTSFaaS/single_node" && pwd)"
source "$HERE/cluster.env"
ROLE="${1:?usage: run-role.sh <database|driver|worker|client> [workerId]}"
WID="${2:-0}"
DOCKER="${DOCKER:-sudo docker}"
RTFAAS_DIR="${RTFAAS_DIR:-/opt/myapp/compare_system/RTSFaaS}"
APP="${APP:-MediaReview}"
die() { printf '\033[1;31m[run-role] %s\033[0m\n' "$*" >&2; exit 1; }

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
WORKER_HOSTS=$(IFS=,; echo "${wnames[*]}")
WORKER_PORTS=$(IFS=,; echo "${wports[*]}")

sudo modprobe rdma_ucm 2>/dev/null || true
[ -n "$($DOCKER images rtfaas:1.0 -q 2>/dev/null)" ] || die "rtfaas:1.0 not built on this node — run: bash setup-node.sh"

# Build an identical RTSFaaS Env on every node (only gatewayHost=localhost is
# node-local; the client that uses it runs on the coordinator).
WORK="$HERE/.role_$(hostname)"; rm -rf "$WORK"; mkdir -p "$WORK/Env" "$WORK/DockerFiles"
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

case "$ROLE" in
  database) cname=rt-database; script=database.sh; args=(database) ;;
  driver)   cname=rt-driver;   script=driver.sh;   args=(driver) ;;
  client)   cname=rt-client;   script=client.sh;   args=(client) ;;
  worker)   cname=rt-worker;   script=worker.sh;   args=(worker "$WID") ;;
  *) die "unknown role: $ROLE" ;;
esac
$DOCKER rm -f "$cname" >/dev/null 2>&1
echo "[run-role] $ROLE on $(hostname) (coord=$COORD_NAME workers=$WORKER_HOSTS num=$WORKER_NUM app=$APP)"
exec $DOCKER run --rm --name "$cname" --network host $ENVS $(mounts "$script") $DEV rtfaas:1.0 "${args[@]}"
