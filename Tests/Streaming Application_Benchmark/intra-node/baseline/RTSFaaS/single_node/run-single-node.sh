#!/usr/bin/env bash
# run-single-node.sh — all 4 RTSFaaS roles on THIS node over RoCE loopback
# (database+driver+worker+client), in-memory store. Self-contained: uses the
# Env/ + DockerFiles/ committed alongside this script. Needs rtfaas:1.0 built here.
set -uo pipefail
WD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; cd "$WD"
sudo modprobe rdma_ucm 2>/dev/null || true
M() { echo "-v $WD/DockerFiles/$1:/rtfaas/$1 -v $WD/DockerFiles/entrypoint.sh:/rtfaas/entrypoint.sh"; }
DEV="--privileged --device=/dev/infiniband/"
run_app() {
  local APP=$1
  local ENVS="--env-file Env/Cluster.env --env-file Env/System.env --env-file Env/$APP.env --env-file Env/Database.env"
  sudo docker rm -f rt-driver rt-database rt-worker rt-client >/dev/null 2>&1
  sudo docker run -d --name rt-driver   --network host $ENVS $(M driver.sh)   $DEV rtfaas:1.0 driver   >/dev/null
  sleep 10
  sudo docker run -d --name rt-database --network host $ENVS $(M database.sh) $DEV rtfaas:1.0 database >/dev/null
  sleep 12
  sudo docker run -d --name rt-worker   --network host $ENVS $(M worker.sh)   $DEV rtfaas:1.0 worker 0 >/dev/null
  sleep 12
  sudo docker run -d --name rt-client   --network host $ENVS $(M client.sh)   $DEV rtfaas:1.0 client   >/dev/null
  # wait for client to finish (up to 90s)
  for i in $(seq 1 18); do
    st=$(sudo docker inspect -f '{{.State.Status}}' rt-client 2>/dev/null)
    [ "$st" = "exited" ] && break
    sleep 5
  done
  echo "######## RTSFaaS $APP ########"
  echo "--- client (end-to-end) ---"; sudo docker logs rt-client 2>&1 | grep -aE "Throughput|Latency" | grep -vaiE "PrintGC"
  echo "--- worker (engine, per-executor k events/s) ---"; sudo docker logs rt-worker 2>&1 | grep -aE "finished execution and exit with throughput" | grep -oE "throughput .*on node: 0" | head -8
  echo "--- driver ---"; sudo docker logs rt-driver 2>&1 | grep -aE "finished with throughput|average latency|99th latency"
}
run_app MediaReview
run_app SocialNetwork
sudo docker rm -f rt-driver rt-database rt-worker rt-client >/dev/null 2>&1
echo "==== RTSFAAS RUNS DONE ===="
