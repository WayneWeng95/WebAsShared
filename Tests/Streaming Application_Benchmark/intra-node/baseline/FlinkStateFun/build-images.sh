#!/usr/bin/env bash
# build-images.sh — (re)generate protobuf + build the four container images for
# the Flink StateFun streaming-application baseline:
#
#   streaming-example-statefun        (Flink StateFun master/worker + module.yaml)
#   streaming-example-mediareview     (MediaReview remote function)
#   streaming-example-socialnetwork   (SocialNetwork remote function)
#   streaming-example-producer        (Kafka driver: seed/replay/gate/throughput)
#
# Usage:
#   ./build-images.sh                 # build locally
#   REGISTRY=myrepo ./build-images.sh # tag/push as $REGISTRY/<image>:latest
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"
REGISTRY="${REGISTRY:-}"

log() { printf '\033[1;36m[build]\033[0m %s\n' "$*"; }

# 1. Regenerate messages_pb2.py from the proto (committed, but keep it fresh).
if command -v protoc >/dev/null 2>&1; then
  log "protoc -> protobuf/messages_pb2.py"
  protoc -I protobuf --python_out=protobuf protobuf/messages.proto
else
  log "protoc not found; using committed protobuf/messages_pb2.py"
fi

build() {
  local image="$1" dockerfile="$2"
  local tag="$image"
  [ -n "$REGISTRY" ] && tag="$REGISTRY/$image:latest"
  log "docker build -f $dockerfile -t $tag ."
  docker build -f "$dockerfile" -t "$tag" .
  if [ -n "$REGISTRY" ]; then
    log "docker push $tag"
    docker push "$tag"
  fi
}

build streaming-example-statefun      docker/Dockerfile.statefun
build streaming-example-mediareview   docker/Dockerfile.mediareview
build streaming-example-socialnetwork docker/Dockerfile.socialnetwork
build streaming-example-producer      docker/Dockerfile.producer

log "done. If you pushed to a registry, update image: names in deployment/k8s/*.yaml."
