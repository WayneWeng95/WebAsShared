#!/usr/bin/env python3
"""analyze_ml_training.py — recompute makespan vs. TOTAL execution time for SGD.

The ml_training sibling of analyze_finra.py / analyze_exec.py. Same two
complementary metrics, because wall-clock makespan structurally favours the policy
that spreads across more nodes (it's a max over nodes), which is misleading when
comparing pack (few nodes) against balanced (many).

  makespan_ms      end-to-end latency = coordinator "total wall time" (≈ the slowest
                   node, usually node 0 which also runs the update/validate reducer
                   on top of its share of gradient workers).
  total_exec_ms    Σ per-node busy time over the WORKING nodes only — the actual
                   resource cost / node-seconds. IDLE nodes (0 gradient workers, e.g.
                   pack's spare nodes) are EXCLUDED.

Working vs idle is read from each cell's cluster_dag (nodes with ≥1 sgd_grad). If the
dag is missing it falls back to a time threshold (idle ≈ <25 % of the max).

Reads logs_ml_training/ml_training__<policy>__<size>__f<fan>.rep*.log +
ml_training_dags/<tag>.json, medians across reps, prints a per-policy table and
writes results_ml_training_exec.csv.
"""
import csv
import glob
import json
import os
import re
import statistics
import collections

HERE = os.path.dirname(os.path.abspath(__file__))
EXP = os.path.dirname(HERE)  # the Scheduling_Policy experiment dir (logs/dags live here)
LOGDIR = os.path.join(EXP, "logs_ml_training")
CDIR = os.path.join(EXP, "ml_training_dags")

ROOT = os.path.abspath(os.path.join(EXP, "..", ".."))
NODE_RE = re.compile(r"node\s+(\d+)\s+\([^)]*\):\s+(\d+)ms", re.I)
WALL_RE = re.compile(r"total wall time:\s+(\d+)ms", re.I)
_SAMPLES = {}  # size stem -> sample count (data rows), measured lazily


def sample_count(size: str) -> int:
    if size not in _SAMPLES:
        p = os.path.join(ROOT, "TestData", "ml", size + ".csv")
        try:
            with open(p) as f:
                _SAMPLES[size] = max(0, sum(1 for _ in f) - 1)
        except OSError:
            # Fall back to the trailing integer in the filename (sgd_<N>).
            m = re.search(r"(\d+)$", size)
            _SAMPLES[size] = int(m.group(1)) if m else 0
    return _SAMPLES[size]


def parse_tag(tag: str):
    # ml_training__<policy>__<size>__f<fanout>  (ignores gt__<size> ground-truth logs)
    m = re.match(r"ml_training__(?P<pol>[a-z]+)__(?P<size>.+?)__f(?P<fan>\d+)$", tag)
    if not m:
        return None
    return m.group("pol"), m.group("size"), int(m.group("fan"))


def working_nodes(tag: str, times: dict) -> set:
    """Node ids that actually ran gradient workers (idle nodes excluded)."""
    cd = os.path.join(CDIR, tag + ".json")
    if os.path.exists(cd):
        nd = json.load(open(cd))["node_dags"]
        work = set()
        for k, nodes in nd.items():
            if any(isinstance(n.get("kind"), dict)
                   and n["kind"].get("WasmVoid", {}).get("func") == "sgd_grad"
                   for n in nodes):
                work.add(int(k))
        if work:
            return work
    # Fallback: a node is "working" if its time is ≥ 25% of the max (idle ≈ 0.5s).
    if not times:
        return set()
    hi = max(times.values())
    return {n for n, t in times.items() if t >= 0.25 * hi}


def main():
    cells = collections.defaultdict(list)  # tag -> list of (makespan, total_exec, idle_excl, nwork)
    skipped = 0
    for log in sorted(glob.glob(os.path.join(LOGDIR, "*.rep*.log"))):
        tag = re.sub(r"\.rep\d+\.log$", "", os.path.basename(log))
        text = open(log).read()
        # Only count reps that SUCCEEDED — a failed job exits early, so its node
        # timings are meaningless and would otherwise make the cell look fast.
        if not re.search(r"Success:\s*true", text, re.I):
            skipped += 1
            continue
        times = {int(n): int(ms) for n, ms in NODE_RE.findall(text)}
        if not times:
            continue
        wall = WALL_RE.search(text)
        makespan = int(wall.group(1)) if wall else max(times.values())
        work = working_nodes(tag, times)
        total_exec = sum(t for n, t in times.items() if n in work)
        idle_excl = sum(t for n, t in times.items() if n not in work)
        cells[tag].append((makespan, total_exec, idle_excl, len(work)))

    rows = []
    for tag, reps in cells.items():
        parsed = parse_tag(tag)
        if not parsed:
            continue
        pol, size, fan = parsed
        med = lambda i: round(statistics.median(r[i] for r in reps))
        rows.append({
            "samples": sample_count(size), "corpus": size, "fanout": fan, "policy": pol,
            "working_nodes": reps[0][3],
            "makespan_ms": med(0), "total_exec_ms": med(1), "idle_excluded_ms": med(2),
            "reps": len(reps),
        })

    rows.sort(key=lambda r: (r["samples"], r["fanout"], r["policy"]))
    out = os.path.join(HERE, "results_ml_training_exec.csv")
    cols = ["samples", "corpus", "fanout", "policy", "working_nodes",
            "makespan_ms", "total_exec_ms", "idle_excluded_ms", "reps"]
    with open(out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols); w.writeheader(); w.writerows(rows)

    # Pretty table grouped by (samples, fanout), pack vs balanced side by side.
    g = collections.OrderedDict()
    for r in rows:
        g.setdefault((r["samples"], r["fanout"]), {})[r["policy"]] = r
    print(f"{'samples':>8} {'fan':>4} | {'policy':<9} {'nodes':>5} {'makespan':>9} {'total_exec':>11} {'idle_excl':>9}")
    print("-" * 72)
    for (sm, fan), d in g.items():
        for pol in ("pack", "balanced", "spread", "random"):
            if pol not in d:
                continue
            r = d[pol]
            print(f"{sm:>8} {fan:>4} | {pol:<9} {r['working_nodes']:>5} "
                  f"{r['makespan_ms']:>7}ms {r['total_exec_ms']:>9}ms {r['idle_excluded_ms']:>7}ms")
        if "pack" in d and "balanced" in d:
            mk = d["balanced"]["makespan_ms"] / d["pack"]["makespan_ms"]
            te = d["balanced"]["total_exec_ms"] / d["pack"]["total_exec_ms"]
            print(f"{'':>15} → makespan {mk:.2f}× | total_exec {te:.2f}×  (balanced/pack)")
    if skipped:
        print(f"\n(excluded {skipped} failed rep-log(s) — Success:false)")
    print(f"wrote {out}")


if __name__ == "__main__":
    raise SystemExit(main())
