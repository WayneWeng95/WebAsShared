#!/usr/bin/env bash
# run.sh — RMMap (dmerge) WordCount, ES protocol sweep on this single box.
# Builds the dmerge-wordcount ES image (stock python:3.7, no kernel module),
# runs a dedicated Redis container, and sweeps the mapper count N — one fresh
# container per N (clean per-N peak RSS) — into results.csv.
#
# Columns: size_mb,workers,topo,e2e_ms,throughput_mb_s,words_per_s,peak_mem_mb,
#          exec_ms,sd_ms,es_ms,kvs_ser_mb,total_occurrences
#   sd_ms = pickle serialize/deserialize, es_ms = Redis round-trip — the
#   serialized inter-stage transfer our zero-copy page-chain reports as 0.
#
# Usage: ./run.sh [CORPUS_NAME] [N_LIST]
#   ./run.sh corpus_large.txt "1 2 4 8 16"
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS_ROOT="$(cd "$HERE/../../../../.." && pwd)"

CORPUS_NAME="${1:-corpus_large.txt}"
N_LIST="${2:-1 2 4 8 16}"
CORPUS="$WAS_ROOT/TestData/$CORPUS_NAME"
IMG="dmerge-wordcount-es:local"
NET="dmerge-net"
CSV="$HERE/results.csv"

[ -f "$CORPUS" ] || { echo "corpus not found: $CORPUS" >&2; exit 1; }

echo "[rmmap-es] building image..."
docker build -q -t "$IMG" "$HERE" >/dev/null

docker network create "$NET" 2>/dev/null || true
if ! docker ps --format '{{.Names}}' | grep -q '^dmerge-redis$'; then
  docker rm -f dmerge-redis 2>/dev/null || true
  docker run -d --name dmerge-redis --network "$NET" redis:7 \
    redis-server --requirepass redis --save "" --maxmemory 8gb >/dev/null
  sleep 2
fi

echo "[rmmap-es] corpus=$CORPUS_NAME  N=[$N_LIST]"
first=1
: > "$CSV"
for N in $N_LIST; do
  hdr=""; [ "$first" = 1 ] && hdr="--header"
  docker run --rm --network "$NET" \
    -e REDIS_HOST=dmerge-redis -e REDIS_PASSWORD=redis \
    -e WC_CORPUS=/data/corpus.txt \
    -v "$CORPUS":/data/corpus.txt:ro \
    "$IMG" "$N" $hdr | tee -a "$CSV"
  first=0
done
echo "[rmmap-es] wrote $CSV"
