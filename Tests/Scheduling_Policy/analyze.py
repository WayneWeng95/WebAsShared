#!/usr/bin/env python3
"""analyze.py — join the scheduling DECISION (placement.csv) with the measured
OUTCOME (results.csv) and print one table per workload.

    placement.csv  ← prepartition.sh   (imbalance, cross-node edges — deterministic)
    results.csv    ← run_sweep.sh       (end-to-end latency on the live cluster)

Writes summary.csv (the joined rows) and prints, per workload, a table sorted by
latency plus the pack-vs-balanced delta — the headline of the experiment.

Run after prepartition.sh (always) and run_sweep.sh (for the latency columns).
placement.csv alone is enough to show the policies make different DECISIONS even
before you have a cluster.
"""
import csv
import os

HERE = os.path.dirname(os.path.abspath(__file__))
POLICY_ORDER = {"pack": 0, "spread": 1, "random": 2, "balanced": 3}


def load_csv(name):
    path = os.path.join(HERE, name)
    if not os.path.exists(path):
        return None
    with open(path) as f:
        return list(csv.DictReader(f))


def main():
    placement = load_csv("placement.csv")
    results = load_csv("results.csv")
    if placement is None:
        print("placement.csv not found — run ./prepartition.sh first.")
        return 1

    # key (workload, policy) -> latency row
    perf = {}
    if results:
        for r in results:
            perf[(r["workload"], r["policy"])] = r

    joined = []
    for p in placement:
        key = (p["workload"], p["policy"])
        r = perf.get(key, {})
        joined.append({
            "workload": p["workload"],
            "policy": p["policy"],
            "busy_hosts": p["busy_hosts"],
            "imbalance": p["imbalance"],
            "cross_node_edges": p["cross_node_edges"],
            "compute_per_host": p["compute_per_host"],
            "wall_ms_median": r.get("wall_ms_median", ""),
            "maxcompute_ms_median": r.get("maxcompute_ms_median", ""),
            "success": r.get("success", ""),
        })

    # summary.csv
    out = os.path.join(HERE, "summary.csv")
    cols = ["workload", "policy", "busy_hosts", "imbalance", "cross_node_edges",
            "compute_per_host", "wall_ms_median", "maxcompute_ms_median", "success"]
    with open(out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        w.writerows(joined)

    workloads = []
    for j in joined:
        if j["workload"] not in workloads:
            workloads.append(j["workload"])

    def lat(j):
        try:
            return float(j["wall_ms_median"])
        except (ValueError, TypeError):
            return None

    for wl in workloads:
        rows = [j for j in joined if j["workload"] == wl]
        have_lat = any(lat(j) is not None for j in rows)
        rows.sort(key=lambda j: (lat(j) if lat(j) is not None else 1e18,
                                 POLICY_ORDER.get(j["policy"], 9)))
        print(f"\n=== {wl} ===")
        print(f"{'policy':<9} {'busy':>5} {'imbal':>6} {'edges':>6} "
              f"{'compute/host':<16} {'wall_ms':>9} {'maxcomp_ms':>11} {'ok':>5}")
        for j in rows:
            ms = j["wall_ms_median"] or "-"
            comp = j["maxcompute_ms_median"] or "-"
            print(f"{j['policy']:<9} {j['busy_hosts']:>5} {j['imbalance']:>6} "
                  f"{j['cross_node_edges']:>6} {j['compute_per_host']:<16} "
                  f"{ms:>9} {comp:>11} {j['success'] or '-':>5}")

        if have_lat:
            by_pol = {j["policy"]: lat(j) for j in rows if lat(j) is not None}
            if "pack" in by_pol and "balanced" in by_pol and by_pol["pack"]:
                delta = (by_pol["balanced"] - by_pol["pack"]) / by_pol["pack"] * 100.0
                faster = "balanced" if delta > 0 else "pack"
                print(f"  → balanced vs pack: {delta:+.1f}%  "
                      f"({faster} is faster on {wl})")

    print(f"\nwrote {out}")
    if not results:
        print("(no results.csv yet — placement decisions shown; run ./run_sweep.sh "
              "on the cluster for the latency columns.)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
