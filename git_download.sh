#!/usr/bin/env bash
#
# git_download.sh — reproduce this node's comparison-system baselines on a new machine.
#
# Clones the six vendored comparison systems into $COMPARE_SYSTEM_DIR
# (default /opt/myapp/compare_system), pins each to the exact commit used on the
# reference node, then re-applies our local benchmark ports (tracked-file patches +
# untracked port source files) recorded under:
#     Tests/Inter-Node deployment/{patches,port_files}/
#
# Safe to re-run (idempotent): existing clones are fetched + re-pinned, patches are
# skipped if already applied, port files are overwritten in place.
#
# Usage:
#   ./git_download.sh                 # clone + patch into /opt/myapp/compare_system
#   COMPARE_SYSTEM_DIR=/path ./git_download.sh
#   ./git_download.sh --clone-only    # clone + pin only, skip our local changes
#   ./git_download.sh --help
#
# See Tests/Inter-Node deployment/compare_system_change.md for the full rationale.

set -uo pipefail

# --------------------------------------------------------------------------- #
# Config
# --------------------------------------------------------------------------- #
TARGET="${COMPARE_SYSTEM_DIR:-/opt/myapp/compare_system}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECORD_DIR="$SCRIPT_DIR/Tests/Inter-Node deployment"
PATCH_DIR="$RECORD_DIR/patches"
PORT_DIR="$RECORD_DIR/port_files"

CLONE_ONLY=0
case "${1:-}" in
  --help|-h)
    # print only the contiguous leading comment block (stop at first non-# line)
    awk 'NR==1{next} /^#/{sub(/^# ?/,""); print; next} {exit}' "$0"
    exit 0 ;;
  --clone-only) CLONE_ONLY=1 ;;
  "") ;;
  *) echo "unknown arg: $1 (try --help)"; exit 2 ;;
esac

# name | clone URL | pinned commit
REPOS=(
  "roadrunner|https://github.com/polaris-slo-cloud/roadrunner.git|b78029d97bd2a05c4155d81570bb2495395074f4"
  "RMMap|https://github.com/ProjectMitosisOS/dmerge-eurosys24-ae.git|8eda3806d5bfd55cc118549cfdf4524434bf170e"
  "RTSFaaS|https://github.com/CGCL-codes/RTSFaaS.git|89d7b65b58cd65b6aa69fe2f429793810e7ba766"
  "cloudburst|https://github.com/hydro-project/cloudburst.git|07905ae3f489fb2a9b920f701c3023c02ee8b877"
  "faasm|https://github.com/faasm/faasm.git|15f3f3a6d220f08882ba7e7165e9618bc2a25de8"   # == tag v0.2.4
  "flink-statefun-transactions|https://github.com/delftdata/flink-statefun-transactions.git|e13327696c1e609730a14c867955385756cda897"
)

FAILURES=()
note()  { printf '  \033[0;36m%s\033[0m\n' "$*"; }
ok()    { printf '  \033[0;32m+ %s\033[0m\n' "$*"; }
skip()  { printf '  \033[0;33m= %s\033[0m\n' "$*"; }
err()   { printf '  \033[0;31m! %s\033[0m\n' "$*"; FAILURES+=("$*"); }
hdr()   { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# --------------------------------------------------------------------------- #
# Clone + pin
# --------------------------------------------------------------------------- #
clone_repo() {
  local name="$1" url="$2" pin="$3" dir="$TARGET/$name"
  hdr "$name"
  if [ -d "$dir/.git" ]; then
    note "present — fetching origin"
    git -C "$dir" fetch --quiet --tags origin || note "(fetch failed; using local objects)"
  else
    note "cloning $url"
    if [ "$name" = "faasm" ]; then
      git clone --recurse-submodules "$url" "$dir" || { err "$name: clone failed"; return 1; }
    else
      git clone "$url" "$dir" || { err "$name: clone failed"; return 1; }
    fi
  fi
  if git -C "$dir" checkout --quiet "$pin"; then
    ok "checked out $pin"
  else
    err "$name: checkout $pin failed"; return 1
  fi
  if [ "$name" = "faasm" ]; then
    note "syncing submodules (faasm is submodule-heavy)"
    git -C "$dir" submodule update --init --recursive || err "faasm: submodule update failed"
  fi
}

# --------------------------------------------------------------------------- #
# Re-apply our local changes
# --------------------------------------------------------------------------- #
apply_patch() {  # <repo_dir> <patch_file>
  local dir="$1" patch="$2" base; base="$(basename "$patch")"
  [ -f "$patch" ] || { err "patch missing: $base"; return 1; }
  if git -C "$dir" apply --reverse --check "$patch" 2>/dev/null; then
    skip "patch already applied: $base"
  elif git -C "$dir" apply --check "$patch" 2>/dev/null; then
    git -C "$dir" apply "$patch" && ok "applied patch: $base" || err "apply failed: $base"
  else
    err "patch will not apply (tree drift?): $base"
  fi
}

copy_in() {  # <src> <dest>  (creates dest parent dirs)
  local src="$1" dest="$2"
  [ -e "$src" ] || { err "port file missing: $src"; return 1; }
  mkdir -p "$(dirname "$dest")"
  cp -a "$src" "$dest" && ok "copied $(basename "$dest")" || err "copy failed: $dest"
}

apply_changes() {
  [ "$CLONE_ONLY" -eq 1 ] && { hdr "skipping local changes (--clone-only)"; return; }

  hdr "RMMap — WordCount port"
  local rm="$TARGET/RMMap/exp/wordcount/app"
  copy_in "$PORT_DIR/RMMap_exp_wordcount_app/functions.py"     "$rm/functions.py"
  copy_in "$PORT_DIR/RMMap_exp_wordcount_app/util.py"          "$rm/util.py"
  copy_in "$PORT_DIR/RMMap_exp_wordcount_app/bindings.pyx"     "$rm/bindings.pyx"
  copy_in "$PORT_DIR/RMMap_exp_wordcount_app/setup.py"         "$rm/setup.py"
  copy_in "$PORT_DIR/RMMap_exp_wordcount_app/native/wrapper.h" "$rm/native/wrapper.h"
  note "REBUILD NEEDED: cd $TARGET/RMMap/pyx && python setup.py build_ext --inplace"

  hdr "RTSFaaS — keep-alive + bounded metrics"
  apply_patch "$TARGET/RTSFaaS" "$PATCH_DIR/RTSFaaS_rdma_metrics.patch"
  note "REBUILD NEEDED: cd $TARGET/RTSFaaS && mvn -q package  (regenerates morph-core jars)"

  hdr "cloudburst — WordCount + TeraSort on Redis"
  apply_patch "$TARGET/cloudburst" "$PATCH_DIR/cloudburst_server.py.patch"
  local cb="$TARGET/cloudburst/cloudburst/server/benchmarks"
  copy_in "$PORT_DIR/cloudburst_benchmarks/wordcount.py"    "$cb/wordcount.py"
  copy_in "$PORT_DIR/cloudburst_benchmarks/terasort.py"     "$cb/terasort.py"
  copy_in "$PORT_DIR/cloudburst_benchmarks/redis_runner.py" "$cb/redis_runner.py"
  copy_in "$PORT_DIR/cloudburst_benchmarks/redis_kvs.py"    "$TARGET/cloudburst/cloudburst/shared/redis_kvs.py"

  hdr "faasm — WordCount Faaslet"
  apply_patch "$TARGET/faasm" "$PATCH_DIR/faasm_func_CMakeLists.txt.patch"
  copy_in "$PORT_DIR/faasm_func_wordcount/wordcount.cpp"  "$TARGET/faasm/func/wordcount/wordcount.cpp"
  copy_in "$PORT_DIR/faasm_func_wordcount/CMakeLists.txt" "$TARGET/faasm/func/wordcount/CMakeLists.txt"
  note "BUILD with the faasm wasm toolchain, then AOT-compile (wc.cwasm)"

  hdr "roadrunner — payload server perms"
  if [ -f "$TARGET/roadrunner/docs/quick-install.sh" ]; then
    chmod +x "$TARGET/roadrunner/docs/quick-install.sh" && ok "chmod +x docs/quick-install.sh"
  else
    err "roadrunner: docs/quick-install.sh not found"
  fi
  note "REGEN NEEDED (not archived — large): bash experiments/evaluation/input-data/make-payloads.sh"
  note "          and: cargo build --release in experiments/evaluation/input-data/storage/"

  hdr "flink-statefun-transactions"
  note "pristine upstream — no local changes to apply"
}

# --------------------------------------------------------------------------- #
# Main
# --------------------------------------------------------------------------- #
echo "Target dir : $TARGET"
echo "Record dir : $RECORD_DIR"
[ -d "$PATCH_DIR" ] || echo "WARNING: $PATCH_DIR not found — patches will fail (run from the WebAsShared tree)."
mkdir -p "$TARGET"

for entry in "${REPOS[@]}"; do
  IFS='|' read -r name url pin <<< "$entry"
  # strip any trailing comment after the pin (e.g. '... # == tag v0.2.4')
  pin="${pin%% *}"
  clone_repo "$name" "$url" "$pin"
done

apply_changes

hdr "Summary"
if [ "${#FAILURES[@]}" -eq 0 ]; then
  echo "All steps completed cleanly."
  echo "Remember the per-repo REBUILD/REGEN steps printed above (RMMap pyx, RTSFaaS jars, faasm wasm, roadrunner payloads)."
  exit 0
else
  echo "Completed with ${#FAILURES[@]} problem(s):"
  for f in "${FAILURES[@]}"; do echo "  - $f"; done
  exit 1
fi
