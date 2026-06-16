#!/usr/bin/env bash
# run.sh — RMMap-ES FINRA sweep (8 rules as parallel processes over Redis+pickle).
# Builds the image + uses the dmerge-redis container; one fresh container per
# trades file → results.csv.
#   ./run.sh "10000 100000 1000000"
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAS="$(cd "$HERE/../../../../.." && pwd)"
SIZES="${1:-10000 100000 1000000}"
IMG="dmerge-finra-es:local"; NET="dmerge-net"; CSV="$HERE/results.csv"

echo "[finra-rmmap] building $IMG..."
docker build -q -t "$IMG" "$HERE" >/dev/null
docker network create "$NET" 2>/dev/null || true
if ! docker ps --format '{{.Names}}' | grep -q '^dmerge-redis$'; then
  docker run -d --name dmerge-redis --network "$NET" redis:7 \
    redis-server --requirepass redis --save "" --maxmemory 8gb >/dev/null
  sleep 2
fi

first=1; : > "$CSV"
for n in $SIZES; do
  f="$WAS/TestData/finra/trades_${n}.csv"
  [ -f "$f" ] || { echo "skip (missing) $f"; continue; }
  hdr=""; [ "$first" = 1 ] && hdr="--header"
  docker run --rm --network "$NET" -e REDIS_HOST=dmerge-redis -e REDIS_PASSWORD=redis \
    -e WC_CORPUS=/data/trades.csv -v "$f":/data/trades.csv:ro \
    "$IMG" $hdr | tee -a "$CSV"; first=0
done
echo "[finra-rmmap] wrote $CSV"
