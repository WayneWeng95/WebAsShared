#!/usr/bin/env bash
# build-image.sh — build the rtfaas:1.0 docker image ON THIS NODE, from the
# git-distributed RTSFaaS source. Run once on every node (the jar is too large to
# git-distribute, so we regenerate it here with Maven instead of save/load-ing the
# image).
#
# What it does, in order:
#   1. apply the single-node RDMABenchmark patch (fix #1) — it is UNCOMMITTED in the
#      source repo, so a fresh `git pull` does NOT carry it; without it the database
#      role's JVM exits and the in-memory store vanishes. Applied idempotently.
#   2. `mvn package` to regenerate the gitignored jars (morph-clients + deps).
#      The native libs (morph-lib/lib/*.so) and role scripts ARE in git already.
#   3. `docker build -t rtfaas:1.0`.
#   (fixes #2-#7 are env/script-level — supplied at `docker run` via our mounted
#    cluster/Env + cluster/DockerFiles — so they are NOT needed in the image.)
#
# Usage:   ./build-image.sh [--force]
#   RTSRC=/path/to/RTSFaaS ./build-image.sh     # override source location
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/cluster.env"                       # IMAGE, DOCKER
RTSRC="${RTSRC:-/opt/myapp/compare_system/RTSFaaS}"
FORCE="${1:-}"
log() { printf '\033[1;35m[build-image %s]\033[0m %s\n' "$(hostname)" "$*"; }
die() { printf '\033[1;31m[build-image] %s\033[0m\n' "$*" >&2; exit 1; }

[ -d "$RTSRC" ] || die "RTSFaaS source not found at $RTSRC (set RTSRC=...)"
[ -f "$RTSRC/Dockerfile" ] || die "$RTSRC/Dockerfile missing"
command -v mvn >/dev/null || die "maven missing — apt-get install -y maven openjdk-8-jdk"
java -version 2>&1 | grep -q '"1\.8' || log "WARN: JDK 8 expected (single-node toolchain); continuing"
$DOCKER version >/dev/null 2>&1 || die "docker not usable as: $DOCKER"

if $DOCKER image inspect "$IMAGE" >/dev/null 2>&1 && [ "$FORCE" != "--force" ]; then
  log "$IMAGE already present on this node — nothing to do (use --force to rebuild)"; exit 0
fi

# 1. fix #1 — idempotent RDMABenchmark patch
RDMA="$RTSRC/morph-clients/src/main/java/benchmark/rdma/RDMABenchmark.java"
[ -f "$RDMA" ] || die "$RDMA missing"
if grep -q 'currentThread().join(); // keep JVM alive' "$RDMA"; then
  log "fix #1 already present"
else
  log "applying fix #1 (RDMABenchmark database join)"
  awk '{print}
       /LOG\.info\("Database started in/{print "                Thread.currentThread().join(); // keep JVM alive to serve RDMA (single-node baseline)"}' \
    "$RDMA" > "$RDMA.tmp" && mv "$RDMA.tmp" "$RDMA"
  grep -q 'currentThread().join(); // keep JVM alive' "$RDMA" || die "patch did not apply — check $RDMA"
fi

# 2. regenerate the gitignored jars
JAR="$RTSRC/morph-clients/target/rtfaas-jar-with-dependencies.jar"
if [ -f "$JAR" ] && [ "$FORCE" != "--force" ]; then
  log "jar already built ($(du -h "$JAR" | cut -f1)) — skipping mvn (use --force to rebuild)"
else
  log "mvn -q -pl morph-clients -am package -DskipTests  (this is the slow step)"
  ( cd "$RTSRC" && mvn -q -pl morph-clients -am package -DskipTests )
  [ -f "$JAR" ] || die "maven build did not produce $JAR"
fi

# 3. build the image
log "$DOCKER build -t $IMAGE  (context: $RTSRC)"
( cd "$RTSRC" && $DOCKER build --platform=linux/amd64 -t "$IMAGE" . )
$DOCKER image inspect "$IMAGE" >/dev/null 2>&1 || die "image $IMAGE not present after build"
log "DONE — $IMAGE built on $(hostname). Next: sudo modprobe rdma_ucm; ./deploy.sh start"
