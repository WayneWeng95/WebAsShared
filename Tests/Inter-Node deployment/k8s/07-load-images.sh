#!/usr/bin/env bash
# 07-load-images.sh — make the StateFun custom images available to THIS node's
# containerd. Multi-node k8s can't see node 0's local docker images, so each node
# must have them (the "cluster lift" the plan flags). Two modes:
#
#   export   (run ONCE on node 0): docker save each STATEFUN_IMAGE → IMAGE_TARBALL_DIR
#   import   (run on EVERY node, incl. node 0): k3s ctr images import each tarball
#
# Default = import. After `export` on node 0, git-sync / copy IMAGE_TARBALL_DIR to
# the other nodes (or put it on shared storage), then run `import` on each.
# (deploy-all.sh does export+import on node 0 and, if SSH works, import on workers.)
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"
MODE="${1:-import}"
log() { printf '\033[1;36m[images %s]\033[0m %s\n' "$(hostname)" "$*"; }
die() { printf '\033[1;31m[images] %s\033[0m\n' "$*" >&2; exit 1; }
mkdir -p "$IMAGE_TARBALL_DIR"

case "$MODE" in
  export)
    command -v docker >/dev/null 2>&1 || die "docker not found — build the StateFun images first (streaming benchmark)"
    for img in $STATEFUN_IMAGES; do
      docker image inspect "$img" >/dev/null 2>&1 || { log "WARN: image '$img' not built locally — skipping"; continue; }
      log "docker save $img → $IMAGE_TARBALL_DIR/$img.tar"
      docker save "$img:latest" -o "$IMAGE_TARBALL_DIR/$img.tar" || die "save $img failed"
    done
    log "exported. Now copy/git-sync $IMAGE_TARBALL_DIR to every node and run: $0 import"
    ;;
  import)
    command -v k3s >/dev/null 2>&1 || die "k3s not installed on this node (run 01/02 first)"
    shopt -s nullglob
    tars=("$IMAGE_TARBALL_DIR"/*.tar)
    [ ${#tars[@]} -gt 0 ] || die "no tarballs in $IMAGE_TARBALL_DIR — run '$0 export' on node 0 and copy them here"
    for t in "${tars[@]}"; do
      log "k3s ctr images import $(basename "$t")"
      sudo k3s ctr images import "$t" || die "import $t failed"
    done
    log "imported $(echo $STATEFUN_IMAGES | wc -w) image(s) into this node's containerd."
    log "NOTE: set imagePullPolicy: IfNotPresent on the StateFun deployments so k8s uses the local image."
    ;;
  *) die "usage: $0 [export|import]" ;;
esac
