#!/bin/bash
# Generate the medium (~10 MiB) and large (~50 MiB) corpus files used by the
# word-count DAG variants (DAGs/rdma_workload_dag/rdma_word_count_medium_*
# and DAGs/cluster_dag/word_count_medium.json, and similarly _large).
#
# These files are deliberately not tracked in git — they are large and
# deterministic (just tiled copies of TestData/corpus.txt).  Run this
# once after cloning to produce them.

set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DATA_DIR="$ROOT/TestData"
SEED="$DATA_DIR/corpus.txt"

if [ ! -f "$SEED" ]; then
    echo "error: seed corpus not found at $SEED" >&2
    exit 1
fi

SEED_SIZE=$(wc -c < "$SEED")
echo "Seed corpus: $SEED ($SEED_SIZE bytes)"

# Tile count = ceil(target_size / seed_size).
MEDIUM_COPIES=$(( (10 * 1024 * 1024 + SEED_SIZE - 1) / SEED_SIZE ))
LARGE_COPIES=$((  (50 * 1024 * 1024 + SEED_SIZE - 1) / SEED_SIZE ))

echo "Generating corpus_medium.txt (~10 MiB, $MEDIUM_COPIES tiles) ..."
rm -f "$DATA_DIR/corpus_medium.txt"
for _ in $(seq 1 $MEDIUM_COPIES); do cat "$SEED"; done > "$DATA_DIR/corpus_medium.txt"

echo "Generating corpus_large.txt  (~50 MiB, $LARGE_COPIES tiles) ..."
rm -f "$DATA_DIR/corpus_large.txt"
for _ in $(seq 1 $LARGE_COPIES);  do cat "$SEED"; done > "$DATA_DIR/corpus_large.txt"

# xlarge (~500 MiB): used to exercise the auto-partitioner's fan-out at a size
# where the per-node map parallelism actually dominates wall time (the 50 MiB
# large corpus is too small to amortise setup/RDMA and hides the fan-out win).
# We tile corpus_large.txt through `head -c` so the result is EXACTLY 500 MiB
# regardless of the seed size, with a newline between tiles so words don't merge
# across boundaries.
XLARGE_BYTES=$(( 500 * 1024 * 1024 ))
echo "Generating corpus_xlarge.txt (500 MiB) ..."
rm -f "$DATA_DIR/corpus_xlarge.txt"
{ while :; do cat "$DATA_DIR/corpus_large.txt"; echo; done; } 2>/dev/null \
    | head -c "$XLARGE_BYTES" > "$DATA_DIR/corpus_xlarge.txt"

echo ""
echo "Done:"
ls -lh "$DATA_DIR/corpus_medium.txt" "$DATA_DIR/corpus_large.txt" "$DATA_DIR/corpus_xlarge.txt"
