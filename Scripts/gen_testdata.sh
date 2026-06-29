#!/usr/bin/env bash
# ============================================================
# gen_testdata.sh — (re)build the FULL shared TestData/ input set for the
# Inter-Node Application Benchmark, covering ALL six workloads as read by the
# non-WasMem frameworks (Faasm, Cloudburst, RMMap-RDMA, Flink StateFun).
#
# Everything here is DETERMINISTIC (seeded) so every framework reads byte-identical
# inputs and the cross-system correctness gates agree. Run once on NODE 0 — the
# baseline drivers load centrally (Faasm pushes chunks via Redis, Cloudburst loads
# into Redis, RMMap RDMA-stages), so workers do NOT need local copies.
#
# Seed corpus: TestData/corpus.txt (restored from git blob c1479760). WordCount is
# PRE-LOWERCASED here because only the WasMem guest lowercases internally; the
# baselines do not — lowercasing the input makes all systems count identically.
#
# Usage:  ./Scripts/gen_testdata.sh            # full set
#         CORPUS_4GB=0 ./Scripts/gen_testdata.sh   # skip the 4 GiB corpus
# ============================================================
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TD="$ROOT/TestData"
GEN_INTRA="$ROOT/Tests/Intra-Node Application_Benchmark"
SCRIPTS="$ROOT/Scripts"

GREEN='\033[0;32m'; CYAN='\033[0;36m'; NC='\033[0m'
info() { echo -e "${GREEN}[gen-testdata]${NC} $*"; }
step() { echo -e "\n${CYAN}━━ $* ━━${NC}"; }

CORPUS_4GB="${CORPUS_4GB:-1}"

mkdir -p "$TD" "$TD/finra" "$TD/ml" "$TD/terasort"
[ -f "$TD/corpus.txt" ] || { echo "error: seed $TD/corpus.txt missing (restore git blob c1479760)"; exit 1; }

# ── WordCount corpora (lowercased, tiled) ───────────────────────────────────
step "WordCount corpora (lowercased)"
LSEED="$TD/.corpus_lower.txt"
tr '[:upper:]' '[:lower:]' < "$TD/corpus.txt" > "$LSEED"
SEED_BYTES=$(wc -c < "$LSEED")

# Tile <base> into <out> until it reaches exactly <target> bytes. A finite copy
# loop + truncate — NOT `while cat | head -c`, whose SIGPIPE-on-close trips
# `set -e`/`pipefail` and aborts the script.
tile_to() {  # <target_bytes> <out> <base>
  local target="$1" out="$2" base="$3"
  local bsize copies
  bsize=$(wc -c < "$base")
  copies=$(( (target + bsize - 1) / bsize ))
  rm -f "$out"
  for ((i=0; i<copies; i++)); do cat "$base"; done > "$out"
  truncate -s "$target" "$out"
}
info "corpus_large.txt  (50 MiB)";  tile_to $((  50 * 1024 * 1024 )) "$TD/corpus_large.txt"  "$LSEED"
# Big corpora tile from the 50 MiB base (≈10 / ≈82 copies) — fast, vs ~350k seed copies.
info "corpus_xlarge.txt (500 MiB)"; tile_to $(( 500 * 1024 * 1024 )) "$TD/corpus_xlarge.txt" "$TD/corpus_large.txt"
if [ "$CORPUS_4GB" = 1 ]; then
  info "corpus_4gb.txt    (4 GiB)";  tile_to $(( 4 * 1024 * 1024 * 1024 )) "$TD/corpus_4gb.txt" "$TD/corpus_large.txt"
fi

# ── FINRA trades (5M) ───────────────────────────────────────────────────────
step "FINRA trades (5,000,000)"
python3 "$GEN_INTRA/Finra/gen_trades.py" 5000000 "$TD/finra/trades_5000000.csv"
cp -f "$TD/finra/trades_5000000.csv" "$TD/finra_5m.csv"   # RMMap-RDMA path
info "→ finra/trades_5000000.csv (Faasm) + finra_5m.csv (RMMap)"

# ── ML training data ────────────────────────────────────────────────────────
# Training seed = gen_data.py default (1234) so the integer-SGD weight gate is the
# canonical one; inference/test split uses a DISTINCT seed (held-out samples).
step "ML training data (seed 1234)"
python3 "$GEN_INTRA/ML_training/gen_data.py"  600000 "$TD/ml/sgd_600000.csv"     1234
python3 "$GEN_INTRA/ML_training/gen_data.py" 6000000 "$TD/ml_training_6m.csv"    1234

step "ML inference/test data (seed 2024) + model"
python3 "$GEN_INTRA/ML_training/gen_data.py"  600000 "$TD/ml/test_600000.csv"    2024
python3 "$GEN_INTRA/ML_training/gen_data.py" 6000000 "$TD/ml_inference_6m.csv"   2024
python3 "$GEN_INTRA/ML_inference/gen_model.py" "$TD/ml_inference_model.csv"
cp -f "$TD/ml_inference_model.csv" "$TD/ml/infer_model.txt"   # Faasm default path

# ── Matrix (4096²) ──────────────────────────────────────────────────────────
step "Matrix 4096² (expect checksum 1391095867672)"
python3 "$GEN_INTRA/Matrix/gen_matrix.py" 4096 "$TD/matrix_a_4096.bin" "$TD/matrix_b_4096.bin"

# ── TeraSort records ────────────────────────────────────────────────────────
step "TeraSort records (32 MiB + 1.2 GiB)"
python3 "$GEN_INTRA/TeraSort/gen_records.py"   32 "$TD/terasort/records_32mb.txt"
python3 "$GEN_INTRA/TeraSort/gen_records.py" 1200 "$TD/terasort_1.2gb.txt"

rm -f "$LSEED"
step "DONE — TestData/ contents"
ls -lh "$TD" "$TD/finra" "$TD/ml" "$TD/terasort" 2>/dev/null
