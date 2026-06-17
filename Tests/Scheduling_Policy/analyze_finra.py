#!/usr/bin/env python3
"""analyze_finra.py — recompute makespan vs. TOTAL execution time for finra.

The finra sibling of analyze_exec.py (which does the same for word_count). Same
two complementary metrics, because wall-clock makespan structurally favours the
policy that spreads across more nodes (it's a max over nodes), which is misleading
when comparing pack (few nodes) against balanced (many) — especially for finra,
where the headline is the OPPOSITE of word_count: finra is coordination-bound, so
pack (locality, fewest cross-node transfers) wins BOTH makespan and total_exec.

  makespan_ms      end-to-end latency = coordinator "total wall time"
                   (≈ the slowest node, usually node 0 which also runs the
                   merge/reduce on top of its share of audit rules).
  total_exec_ms    Σ per-node busy time over the WORKING nodes only — the actual
                   resource cost / node-seconds. IDLE nodes (0 audit rules, e.g.
                   pack's spare nodes that just spin up for ~0.5s) are EXCLUDED.

Working vs idle is read from each cell's cluster_dag (nodes with ≥1
finra_audit_rule). If the dag is missing it falls back to a time threshold
(idle ≈ <25 % of the max).

Reads logs_finra/finra__<policy>.rep*.log + finra_dags/finra__<policy>.json,
medians across reps, prints a per-policy table and writes results_finra_exec.csv.
"""
import csv
import glob
import json
import os
import re
import statistics
import collections

HERE = os.path.dirname(os.path.abspath(__file__))
LOGDIR = os.path.join(HERE, "logs_finra")
CDIR = os.path.join(HERE, "finra_dags")

ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
NODE_RE = re.compile(r"node\s+(\d+)\s+\([^)]*\):\s+(\d+)ms", re.I)
WALL_RE = re.compile(r"total wall time:\s+(\d+)ms", re.I)
_TRADES = {}  # size stem -> trade count (data rows), measured lazily


def trade_count(size: str) -> int:
    if size not in _TRADES:
        p = os.path.join(ROOT, "TestData", "finra", size + ".csv")
        try:
            with open(p) as f:
                _TRADES[size] = max(0, sum(1 for _ in f) - 1)
        except OSError:
            # Fall back to the trailing integer in the filename (trades_<N>).
            m = re.search(r"(\d+)$", size)
            _TRADES[size] = int(m.group(1)) if m else 0
    return _TRADES[size]


def parse_tag(tag: str):
    # finra__<policy>__<size>__f<fanout>  (ignores gt__<size> ground-truth logs)
    m = re.match(r"finra__(?P<pol>[a-z]+)__(?P<size>.+?)__f(?P<fan>\d+)$", tag)
    if not m:
        return None
    return m.group("pol"), m.group("size"), int(m.group("fan"))


def working_nodes(tag: str, times: dict) -> set:
    """Node ids that actually ran audit rules (idle nodes excluded)."""
    cd = os.path.join(CDIR, tag + ".json")
    if os.path.exists(cd):
        nd = json.load(open(cd))["node_dags"]
        work = set()
        for k, nodes in nd.items():
            if any(isinstance(n.get("kind"), dict)
                   and n["kind"].get("WasmVoid", {}).get("func") == "finra_audit_rule"
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
    for log in sorted(glob.glob(os.path.join(LOGDIR, "*.rep*.log"))):
        tag = re.sub(r"\.rep\d+\.log$", "", os.path.basename(log))
        text = open(log).read()
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
            "trades": trade_count(size), "corpus": size, "fanout": fan, "policy": pol,
            "working_nodes": reps[0][3],
            "makespan_ms": med(0), "total_exec_ms": med(1), "idle_excluded_ms": med(2),
            "reps": len(reps),
        })

    rows.sort(key=lambda r: (r["trades"], r["fanout"], r["policy"]))
    out = os.path.join(HERE, "results_finra_exec.csv")
    cols = ["trades", "corpus", "fanout", "policy", "working_nodes",
            "makespan_ms", "total_exec_ms", "idle_excluded_ms", "reps"]
    with open(out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols); w.writeheader(); w.writerows(rows)

    # Pretty table grouped by (trades, fanout), pack vs balanced side by side.
    g = collections.OrderedDict()
    for r in rows:
        g.setdefault((r["trades"], r["fanout"]), {})[r["policy"]] = r
    print(f"{'trades':>8} {'fan':>4} | {'policy':<9} {'nodes':>5} {'makespan':>9} {'total_exec':>11} {'idle_excl':>9}")
    print("-" * 72)
    for (tr, fan), d in g.items():
        for pol in ("pack", "balanced", "spread", "random"):
            if pol not in d:
                continue
            r = d[pol]
            print(f"{tr:>8} {fan:>4} | {pol:<9} {r['working_nodes']:>5} "
                  f"{r['makespan_ms']:>7}ms {r['total_exec_ms']:>9}ms {r['idle_excluded_ms']:>7}ms")
        if "pack" in d and "balanced" in d:
            mk = d["balanced"]["makespan_ms"] / d["pack"]["makespan_ms"]
            te = d["balanced"]["total_exec_ms"] / d["pack"]["total_exec_ms"]
            print(f"{'':>15} → makespan {mk:.2f}× | total_exec {te:.2f}×  (balanced/pack)")
    print("\n(finra is coordination-bound: expect pack to win, i.e. ratios ≥ 1)")
    print(f"wrote {out}")


if __name__ == "__main__":
    raise SystemExit(main())
