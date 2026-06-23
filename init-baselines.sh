#!/usr/bin/env bash
# ============================================================
# init-baselines.sh
# One-shot setup for the COMPARABLE / baseline systems on THIS node — the
# counterpart to init-node.sh (which prepares OUR framework, WasMem). It stands up
# the baseline distribution layers the inter-node experiments compare against. Each
# baseline gets a per-node AGENT (the analogue of WasMem's node-agent) so the
# coordinator places roles over HTTP — NO ssh anywhere on this cluster.
#
#   1. Faasm    — per-node Faaslet agent (faasm/agent.py, port 9600). Installs the
#                 runtime (wasmtime) + deps, then STARTS the agent. Started FIRST
#                 because the RTSFaaS rt-agent bootstraps remote workers THROUGH it
#                 (see track 2).  → faasm/setup-node.sh + faasm/deploy.sh start
#
#   2. RTSFaaS  — native Java/Maven track with its OWN dedicated agent. Builds the
#                 rtfaas:1.0 image (Java 8 + Maven + rdma_ucm + the DB-role patch),
#                 then STARTS rt-agent.py (port 9700), which launches the
#                 database/driver/worker/client role containers on its node;
#                 rt-coordinator.py places them. On this no-ssh cluster the
#                 coordinator starts each worker's rt-agent via that worker's faasm
#                 agent (9600) — hence faasm runs first.
#                   → rtsfaas/setup-node.sh + <streaming>/RTSFaaS/cluster/deploy.sh start
#
#   3. Kubernetes — shared k8s runtime for the k8s baselines (Cloudburst,
#                 RMMap/Knative, Flink StateFun): k3s + Knative+Kourier +
#                 Strimzi/Kafka + Redis. Role-aware via k8s/cluster.env.
#                   → k8s/deploy-all.sh
#
# PORT MAP — kept DISJOINT so all the control agents coexist (run the experiments
# one at a time, but the agents themselves stay up): node-agent 9500 · faasm 9600 ·
# rt-agent 9700 · k3s 6443/10250 · Redis 6379 · RTSFaaS roles: database 2381,
# gateway 5556, driver 5559, worker i = 5550+i*10.
#
# Idempotent and role-aware: run it ONCE PER NODE (the repo is git-synced; ssh
# between nodes is publickey-blocked). Run it on the WORKERS first (so their faasm
# agents are up), then on node 0 — node 0's rt-agent bootstrap then reaches every
# worker; either way each node starts its own agents. Topology for every track lives
# in each baseline's cluster.env (default node 0 = 10.10.1.2 coordinator/control-plane).
#
# Usage:
#   ./init-baselines.sh                  # all three tracks
#   ./init-baselines.sh faasm rtsfaas    # only the listed tracks (faasm|rtsfaas|k8s)
#   NO_PULL=1  ./init-baselines.sh       # skip the leading `git pull`
#   NO_START=1 ./init-baselines.sh       # build/install only; don't start any agent
#                                        #   (faasm/rtsfaas: setup but no agent; k8s skipped)
# ============================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"
BASE="$ROOT/Tests/Inter-Node deployment"
# Dedicated RTSFaaS agent/coordinator (rt-agent.py :9700, rt-coordinator.py) lives in
# the streaming benchmark — that is the supported multi-node RTSFaaS substrate.
RT_CLUSTER="$ROOT/Tests/Streaming Application_Benchmark/inter-node/baseline/RTSFaaS/cluster"
# RTSFaaS source repo lives alongside this one: ../compare_system/RTSFaaS.
# setup-node.sh honours RTFAAS_DIR; derive it from the repo root so the track works
# regardless of where the tree is checked out.
RTFAAS_DIR="${RTFAAS_DIR:-$(cd "$ROOT/.." && pwd)/compare_system/RTSFaaS}"
export RTFAAS_DIR

GREEN='\033[0;32m'; BLUE='\033[1;34m'; YELLOW='\033[1;33m'; RED='\033[1;31m'; NC='\033[0m'
info()  { echo -e "${GREEN}[init-baselines]${NC} $*"; }
step()  { echo -e "\n${BLUE}━━ $* ━━${NC}"; }
warn()  { echo -e "${YELLOW}[init-baselines]${NC} $*"; }
die()   { echo -e "${RED}[init-baselines] $*${NC}" >&2; exit 1; }

# Track selection: positional args, default all. Execution order matters — faasm
# before rtsfaas (rt-agent's no-ssh worker bootstrap uses the faasm agent).
TRACKS=("$@")
[ "${#TRACKS[@]}" -eq 0 ] && TRACKS=(faasm rtsfaas k8s)
want() { local t; for t in "${TRACKS[@]}"; do [ "$t" = "$1" ] && return 0; done; return 1; }

[ -d "$BASE" ] || die "baseline scripts not found at \"$BASE\" — wrong repo? (run from WasMem root)"

echo ""
echo "╔══════════════════════════════════════════════╗"
echo "║   Baseline / Comparable Systems Bootstrap    ║"
echo "╚══════════════════════════════════════════════╝"
info "node $(hostname) — tracks: ${TRACKS[*]}  (NO_START=${NO_START:-0})"

# ── Step 0: Git pull (baseline scripts + cluster.env are git-synced) ──────────
if [ "${NO_PULL:-0}" != 1 ]; then
  info "Pulling latest code..."
  git pull || warn "git pull failed (offline / dirty tree?) — continuing with local copy"
fi

# Make every track's scripts executable on a freshly-synced node (they call siblings).
chmod +x "$BASE/rtsfaas/"*.sh "$BASE/faasm/"*.sh "$BASE/k8s/"*.sh 2>/dev/null || true
chmod +x "$RT_CLUSTER/"*.sh "$RT_CLUSTER/"*.py 2>/dev/null || true

# run_track <name> <cmd...> — run a track, record pass/fail, never abort the others.
declare -A RESULT
run_track() {
  local name="$1"; shift
  step "Track: $name"
  if ( "$@" ); then RESULT[$name]="ok"; info "$name: done"
  else RESULT[$name]="FAILED"; warn "$name: FAILED (see output above) — continuing with remaining tracks"; fi
}

# ── Track 1: Faasm — per-node agent (wasmtime runtime + agent.py daemon, :9600) ─
# Runs first: RTSFaaS's rt-agent uses the faasm agent to start remote workers (no ssh).
if want faasm; then
  if [ "${NO_START:-0}" = 1 ]; then
    run_track faasm bash "$BASE/faasm/setup-node.sh"
  else
    run_track faasm bash -c 'set -e; bash "$1/setup-node.sh"; bash "$1/deploy.sh" start; bash "$1/deploy.sh" status' _ "$BASE/faasm"
  fi
fi

# ── Track 2: RTSFaaS — build rtfaas:1.0 image + start the dedicated rt-agent (:9700)
if want rtsfaas; then
  [ -d "$RTFAAS_DIR" ] || warn "RTSFaaS repo not at \"$RTFAAS_DIR\" — git-sync it there or set RTFAAS_DIR (setup-node.sh will error)"
  [ -d "$RT_CLUSTER" ] || warn "rt-agent dir not found at \"$RT_CLUSTER\""
  info "RTSFaaS repo: $RTFAAS_DIR   rt-agent: $RT_CLUSTER"
  if [ "${NO_START:-0}" = 1 ]; then
    run_track rtsfaas bash "$BASE/rtsfaas/setup-node.sh"
  else
    # setup-node.sh builds rtfaas:1.0 (Java/Maven/rdma/patch); cluster/deploy.sh start
    # launches rt-agent.py here and — on node 0 — each worker's rt-agent via its faasm agent.
    run_track rtsfaas bash -c '
      set -e
      bash "$1/rtsfaas/setup-node.sh"
      bash "$2/deploy.sh" start
      bash "$2/deploy.sh" status
    ' _ "$BASE" "$RT_CLUSTER"
  fi
fi

# ── Track 3: Kubernetes — shared runtime for the k8s baselines ────────────────
# deploy-all.sh both installs AND starts (k3s + Knative + Strimzi + Redis); there's
# no separate "install-only" path, so under NO_START we skip it and just point at it.
if want k8s; then
  if [ "${NO_START:-0}" = 1 ]; then
    RESULT[k8s]="skipped"; warn "k8s: skipped (NO_START=1) — start it with: \"$BASE/k8s/deploy-all.sh\""
  else
    run_track k8s bash "$BASE/k8s/deploy-all.sh"
  fi
fi

# ── Summary ───────────────────────────────────────────────────────────────────
step "Summary"
rc=0
for t in "${TRACKS[@]}"; do
  s="${RESULT[$t]:-not-run}"
  case "$s" in
    ok)      echo -e "  ${GREEN}✓${NC} $t" ;;
    skipped) echo -e "  ${YELLOW}–${NC} $t (skipped)" ;;
    *)       echo -e "  ${RED}✗${NC} $t ($s)"; rc=1 ;;
  esac
done

echo ""
info "Next steps (run per experiment):"
want faasm && cat <<EOF
  Faasm   : "$BASE/faasm/deploy.sh" status      then   ./deploy.sh run <job-spec.json>
EOF
want rtsfaas && cat <<EOF
  RTSFaaS : "$RT_CLUSTER/deploy.sh" status       (rt-agents on :9700, all nodes)
            "$RT_CLUSTER/rt-coordinator.py" --both --reps 15   → results_rtsfaas_cluster.csv
EOF
want k8s && cat <<EOF
  k8s     : kubectl get nodes -o wide   then deploy apps from "$BASE/k8s"/{cloudburst,knative,statefun}/
EOF

echo ""
[ "$rc" -eq 0 ] && info "Baseline bootstrap complete." || warn "Baseline bootstrap finished with failures (rc=$rc)."
exit "$rc"
