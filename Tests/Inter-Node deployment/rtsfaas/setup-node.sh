#!/usr/bin/env bash
# setup-node.sh — install + build everything RTSFaaS needs on THIS node.
# Idempotent: each step is skipped if already satisfied. Because the repo is
# git-synced across nodes (source only — not the built jar, the docker image, or
# system packages), run this once per node (manually on each, or the runner ssh's
# it for you with SETUP=1).
#
# Installs/ensures: RDMA CM kernel module, Java 8 + Maven, the DB-role JVM-stays-
# alive source patch, the rtfaas-jar-with-dependencies.jar, and the rtfaas:1.0
# docker image. Then the multi-node runner just launches role containers.
set -uo pipefail
RTFAAS_DIR="${RTFAAS_DIR:-/opt/myapp/compare_system/RTSFaaS}"
JAVA_HOME_8="${JAVA_HOME_8:-/usr/lib/jvm/java-8-openjdk-amd64}"
log() { printf '\033[1;36m[setup %s]\033[0m %s\n' "$(hostname)" "$*"; }
die() { printf '\033[1;31m[setup %s] %s\033[0m\n' "$(hostname)" "$*" >&2; exit 1; }
[ -d "$RTFAAS_DIR" ] || die "RTSFaaS repo not found at $RTFAAS_DIR (git-sync it here, or set RTFAAS_DIR)"

# 1. RDMA connection-manager device (needed every boot; harmless if already loaded)
log "modprobe rdma_ucm"
sudo modprobe rdma_ucm 2>/dev/null || true
[ -e /dev/infiniband/rdma_cm ] && log "rdma_cm present" || log "WARN: /dev/infiniband/rdma_cm missing (no RDMA NIC?)"

# 2. Java 8 + Maven + docker present
need_pkg=0
java -version 2>&1 | grep -q '"1\.8' || need_pkg=1
command -v mvn >/dev/null 2>&1 || need_pkg=1
if [ "$need_pkg" = 1 ]; then
  log "apt-get install openjdk-8-jdk maven"
  sudo apt-get update -qq && sudo apt-get install -y -qq openjdk-8-jdk maven || die "apt install failed"
fi
command -v docker >/dev/null 2>&1 || die "docker not installed on this node"

# 3. DB-role patch: keep the database JVM alive so it serves RDMA (idempotent)
RB="$RTFAAS_DIR/morph-clients/src/main/java/benchmark/rdma/RDMABenchmark.java"
[ -f "$RB" ] || die "RDMABenchmark.java not found ($RB)"
if ! grep -q "Thread.currentThread().join()" "$RB"; then
  log "applying DB-role join() patch to RDMABenchmark.java"
  sed -i 's#\(LOG.info("Database started in " + (end - start - 1) + " ms");\)#\1\n                Thread.currentThread().join(); // keep DB JVM alive to serve RDMA#' "$RB"
  grep -q "Thread.currentThread().join()" "$RB" || die "patch failed"
fi

# 4. Build the fat jar if missing
JAR="$RTFAAS_DIR/morph-clients/target/rtfaas-jar-with-dependencies.jar"
if [ ! -f "$JAR" ]; then
  log "building jar (mvn clean install -DskipTests) — first run downloads deps, ~minutes"
  ( cd "$RTFAAS_DIR" && JAVA_HOME="$JAVA_HOME_8" mvn -q clean install -DskipTests ) || die "mvn build failed"
fi
[ -f "$JAR" ] || die "jar not produced"
log "jar ready ($(du -h "$JAR" | cut -f1))"

# 5. Build the docker image if missing
if ! sudo docker images rtfaas:1.0 -q | grep -q .; then
  log "building docker image rtfaas:1.0 (~2 min)"
  ( cd "$RTFAAS_DIR" && sudo docker build --platform=linux/amd64 -t rtfaas:1.0 . ) || die "docker build failed"
fi
log "rtfaas:1.0 ready ($(sudo docker images rtfaas:1.0 --format '{{.Size}}'))"
log "node setup complete"
