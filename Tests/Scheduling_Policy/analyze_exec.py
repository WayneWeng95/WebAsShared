#!/usr/bin/env python3
"""analyze_exec.py — recompute makespan vs. TOTAL execution time from run logs.

Two complementary metrics, because wall-clock makespan structurally favours the
policy that uses more nodes (it's a max over nodes), which is misleading when
comparing pack (few nodes, more work each) against balanced (many nodes):

  makespan_ms      end-to-end latency  = coordinator "total wall time"
                   (≈ the slowest node, which is usually node 0 since it also
                   runs the convergence/reduce on top of its share of maps).
  total_exec_ms    Σ per-node busy time over the WORKING nodes only — the actual
                   resource cost / node-seconds of work. IDLE nodes (0 map
                   workers, e.g. pack's spare nodes that just spin up + load an
                   empty slice for ~0.5s) are EXCLUDED so they don't pollute it.

Working vs idle is read from each cell's cluster_dag (nodes with ≥1 wc_map). If
the dag is missing it falls back to a time threshold (idle ≈ <25 % of the max).

Reads logs_wc_size/<tag>.rep*.log + cluster_dags/<tag>.json, medians across reps,
prints a per-cell table and writes results_exec.csv.
"""
import csv
import glob
import json
import os
import re
import statistics
import collections

HERE = os.path.dirname(os.path.abspath(__file__))
LOGDIR = os.path.join(HERE, "logs_wc_size")
CDIR = os.path.join(HERE, "cluster_dags")
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))

NODE_RE = re.compile(r"node\s+(\d+)\s+\([^)]*\):\s+(\d+)ms", re.I)
WALL_RE = re.compile(r"total wall time:\s+(\d+)ms", re.I)
CORPUS_MB = {}  # corpus stem -> size_mb (measured lazily)


def corpus_mb(stem: str) -> int:
    if stem not in CORPUS_MB:
        p = os.path.join(ROOT, "TestData", stem + ".txt")
        CORPUS_MB[stem] = round(os.path.getsize(p) / 1048576) if os.path.exists(p) else 0
    return CORPUS_MB[stem]


def parse_tag(tag: str):
    # word_count__<policy>__<corpus_stem>[__f<fanout>]
    m = re.match(r"word_count__(?P<pol>[a-z]+)__(?P<corpus>.+?)(?:__f(?P<fan>\d+))?$", tag)
    if not m:
        return None
    return m.group("pol"), m.group("corpus"), int(m.group("fan") or 0)


def working_nodes(tag: str, times: dict) -> set:
    """Node ids that actually ran map workers (idle nodes excluded)."""
    cd = os.path.join(CDIR, tag + ".json")
    if os.path.exists(cd):
        nd = json.load(open(cd))["node_dags"]
        work = set()
        for k, nodes in nd.items():
            if any(isinstance(n.get("kind"), dict)
                   and n["kind"].get("WasmVoid", {}).get("func") == "wc_map" for n in nodes):
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
        pol, corpus, fan = parsed
        med = lambda i: round(statistics.median(r[i] for r in reps))
        rows.append({
            "size_mb": corpus_mb(corpus), "corpus": corpus, "fanout": fan, "policy": pol,
            "working_nodes": reps[0][3],
            "makespan_ms": med(0), "total_exec_ms": med(1), "idle_excluded_ms": med(2),
            "reps": len(reps),
        })

    rows.sort(key=lambda r: (r["size_mb"], r["fanout"], r["policy"]))
    out = os.path.join(HERE, "results_exec.csv")
    cols = ["size_mb", "corpus", "fanout", "policy", "working_nodes",
            "makespan_ms", "total_exec_ms", "idle_excluded_ms", "reps"]
    with open(out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols); w.writeheader(); w.writerows(rows)

    # Pretty table grouped by (size, fanout), pack vs balanced side by side.
    g = collections.OrderedDict()
    for r in rows:
        g.setdefault((r["size_mb"], r["fanout"]), {})[r["policy"]] = r
    print(f"{'size':>6} {'fan':>4} | {'policy':<9} {'nodes':>5} {'makespan':>9} {'total_exec':>11} {'idle_excl':>9}")
    print("-" * 70)
    for (sz, fan), d in g.items():
        for pol in ("pack", "balanced", "spread", "random"):
            if pol not in d:
                continue
            r = d[pol]
            print(f"{sz:>6} {fan:>4} | {pol:<9} {r['working_nodes']:>5} "
                  f"{r['makespan_ms']:>7}ms {r['total_exec_ms']:>9}ms {r['idle_excluded_ms']:>7}ms")
        if "pack" in d and "balanced" in d:
            mk = d["balanced"]["makespan_ms"] / d["pack"]["makespan_ms"]
            te = d["balanced"]["total_exec_ms"] / d["pack"]["total_exec_ms"]
            print(f"{'':>13}  → makespan {mk:.2f}× | total_exec {te:.2f}×  (balanced/pack)")
    print(f"\nwrote {out}")


if __name__ == "__main__":
    raise SystemExit(main())
