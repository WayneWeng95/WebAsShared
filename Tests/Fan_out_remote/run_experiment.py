#!/usr/bin/env python3
"""
Fan-out sweep experiment for the capacity-aware word-count system.

For each (corpus size, fanout) it submits the word_count_auto_placement DAG to
the running 2-node cluster, parses the per-node + total wall time, and records
total_occurrences for a correctness check (must be constant across fanouts for a
given corpus — fanout changes only parallelism, not the data).

Fanout here is the NEW cluster-total semantics (Point 6): `fanout: N` = N map
workers across the whole cluster, split by capacity (≈N/2 per node on a balanced
2-node cluster), clamped to the cluster core budget (Point 7).

Usage:  python3 Tests/Fan_out_remote/run_experiment.py
Run from the WebAsShared project root with the coordinator already started.
"""
import json, subprocess, re, sys, time, pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]  # WebAsShared/ (file: Tests/Fan_out_remote/)
OUTDIR = ROOT / "Tests/Fan_out_remote"
SYMBOLIC = ROOT / "DAGs/symbolic_dag/word_count_auto_placement.json"
CONFIG = ROOT / "NodeAgent/agent_coordinator.toml"
NODE_AGENT = ROOT / "node-agent"

# (label, path, approx size) — smallest first so failures surface fast.
CORPORA = [
    ("52MB",  "TestData/corpus_large.txt"),
    ("524MB", "TestData/corpus_xlarge.txt"),
    ("1GB",   "TestData/corpus.txt"),
]
FANOUTS = [5, 10, 15, 20]
REPS = 2  # repetitions per cell; we report the best (min) wall time.

def make_dag(fanout: int, corpus: str) -> str:
    d = json.loads(SYMBOLIC.read_text())
    for n in d["nodes"]:
        if n["id"] == "map_n":
            n["fanout"] = fanout
        if "Input" in n["kind"]:
            n["kind"]["Input"]["path"] = corpus
    p = OUTDIR / f"_dag_fo{fanout}.json"
    p.write_text(json.dumps(d, indent=2))
    return str(p)

def submit(dag_path: str):
    """Submit one job; return (ok, node0_ms, node1_ms, total_ms)."""
    out = subprocess.run(
        [str(NODE_AGENT), "submit", "--config", str(CONFIG), "--dag", dag_path],
        capture_output=True, text=True, timeout=600,
    ).stdout
    ok = "Success: true" in out
    def grab(pat):
        m = re.search(pat, out)
        return int(m.group(1)) if m else None
    return (ok,
            grab(r"node 0 \(local\):\s*(\d+)ms"),
            grab(r"node 1 \(worker\):\s*(\d+)ms"),
            grab(r"total wall time:\s*(\d+)ms"))

def total_occurrences() -> int:
    f = ROOT / "TestOutput/wc_auto_placement_result.txt"
    for line in f.read_text().splitlines():
        if line.startswith("total_occurrences"):
            return int(line.split("=")[1])
    return -1

def main():
    rows = []          # (corpus, fanout, best_total_ms, node0_ms, node1_ms, total_occ)
    totals = {}        # corpus -> set of total_occurrences seen (correctness)
    for label, path in CORPORA:
        if not (ROOT / path).exists():
            print(f"!! skip {label}: {path} missing", flush=True)
            continue
        dags = {fo: make_dag(fo, path) for fo in FANOUTS}
        for fo in FANOUTS:
            best = None
            occ = None
            for rep in range(REPS):
                ok, n0, n1, tot = submit(dags[fo])
                occ = total_occurrences()
                totals.setdefault(label, set()).add(occ)
                tag = "ok" if ok else "FAIL"
                print(f"[{label:5s} fanout={fo:2d} rep{rep}] {tag} "
                      f"total={tot}ms node0={n0}ms node1={n1}ms occ={occ}", flush=True)
                if ok and tot is not None and (best is None or tot < best[0]):
                    best = (tot, n0, n1)
            if best:
                rows.append((label, fo, best[0], best[1], best[2], occ))
    # ── Results table ──
    print("\n================ FAN-OUT SWEEP RESULTS (best of %d) ================" % REPS)
    print(f"{'corpus':7s} {'fanout':>6s} {'total_ms':>9s} {'node0_ms':>9s} {'node1_ms':>9s} {'total_occ':>12s}")
    for r in rows:
        print(f"{r[0]:7s} {r[1]:>6d} {r[2]:>9d} {r[3]:>9d} {r[4]:>9d} {r[5]:>12d}")
    # ── Correctness: total_occurrences must be identical across fanouts per corpus ──
    print("\n---- correctness (total_occurrences constant across fanouts) ----")
    for label, occset in totals.items():
        ok = len(occset) == 1
        print(f"{label:7s}: {'OK' if ok else 'MISMATCH'}  values={sorted(occset)}")
    # ── CSV ──
    csv = OUTDIR / "results.csv"
    with csv.open("w") as f:
        f.write("corpus,fanout,total_ms,node0_ms,node1_ms,total_occ\n")
        for r in rows:
            f.write(",".join(str(x) for x in r) + "\n")
    print(f"\nwrote {csv}")

if __name__ == "__main__":
    main()
