#!/usr/bin/env python3
# analyze_scaling.py — merge the four sweep CSVs into one tidy table and
# (re)derive speedup / efficiency canonically from the median makespan, so the
# numbers are independent of how the shell scripts computed them.
#
#   strong:  speedup(p)    = T(p0)/T(p)            ideal = p/p0
#            efficiency(p)  = speedup(p)·p0/p       ideal = 1.0
#   weak:    throughput_scaling(p) = thr(p)/thr(p0)  ideal = p/p0
#            efficiency(p)  = T(p0)/T(p)            ideal = 1.0  (flat makespan)
#
# Reads ../analysis/results_{wc,stream}_{strong,weak}.csv (whichever exist),
# writes results_scaling.csv and prints a per-workload table.
import csv, os

HERE = os.path.dirname(os.path.abspath(__file__))
SOURCES = [
    ("word_count", "strong", "results_wc_strong.csv"),
    ("word_count", "weak",   "results_wc_weak.csv"),
    ("mediareview","strong", "results_stream_strong.csv"),
    ("mediareview","weak",   "results_stream_weak.csv"),
]

def load(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return list(csv.DictReader(f))

def num(row, *keys):
    for k in keys:
        v = row.get(k)
        if v not in (None, "", "NA"):
            try: return float(v)
            except ValueError: pass
    return None

out_rows = []
for workload, mode, fname in SOURCES:
    rows = load(os.path.join(HERE, fname))
    if not rows:
        print(f"[skip] {fname} (not found / empty)")
        continue
    # split into independent series by placement policy (word_count has balanced+pack;
    # streaming has the single "n/a" data-parallel series). Speedup baseline is
    # per-policy: each series is normalized to its OWN smallest-thread point.
    policies = sorted({r.get("policy","n/a") for r in rows})
    for policy in policies:
        srows = sorted((r for r in rows if r.get("policy","n/a")==policy),
                       key=lambda r: int(r["threads"]))
        if not srows: continue
        t0 = int(srows[0]["threads"])
        ms0 = num(srows[0], "wall_ms_median")
        thr0 = num(srows[0], "throughput_mb_s", "throughput_ev_s")
        for r in srows:
            p = int(r["threads"])
            ms = num(r, "wall_ms_median")
            thr = num(r, "throughput_mb_s", "throughput_ev_s")
            speedup = (ms0/ms) if (ms0 and ms) else None
            if mode == "strong":
                ideal_speedup = p / t0
                eff = (speedup * t0 / p) if speedup else None
                thr_scaling = None
            else:
                ideal_speedup = None
                thr_scaling = (thr/thr0) if (thr0 and thr) else None
                eff = (ms0/ms) if (ms0 and ms) else None
            out_rows.append(dict(
                workload=workload, mode=mode, policy=policy, threads=p, nodes=r.get("nodes"),
                wall_ms_median=ms, throughput=thr,
                speedup=round(speedup,3) if speedup else "",
                ideal_speedup=round(ideal_speedup,3) if ideal_speedup else "",
                throughput_scaling=round(thr_scaling,3) if thr_scaling else "",
                efficiency=round(eff,3) if eff else "",
                peak_mem_mb_per_node=num(r, "peak_mem_mb_per_node"),
                peak_mem_mb_total=num(r, "peak_mem_mb_total"),
                success=r.get("success"),
            ))

if out_rows:
    cols = ["workload","mode","policy","threads","nodes","wall_ms_median","throughput",
            "speedup","ideal_speedup","throughput_scaling","efficiency",
            "peak_mem_mb_per_node","peak_mem_mb_total","success"]
    dst = os.path.join(HERE, "results_scaling.csv")
    with open(dst, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols); w.writeheader()
        for r in out_rows: w.writerow(r)
    print(f"\nwrote {dst}\n")
    # pretty print grouped by workload / mode / policy
    seen = []
    for workload, mode, _ in SOURCES:
        for policy in sorted({r["policy"] for r in out_rows
                              if r["workload"]==workload and r["mode"]==mode}):
            key = (workload, mode, policy)
            if key in seen: continue
            seen.append(key)
            grp = [r for r in out_rows if (r["workload"],r["mode"],r["policy"])==key]
            if not grp: continue
            ptag = "" if policy=="n/a" else f" / {policy}"
            print(f"── {workload} / {mode}{ptag} ──")
            print(f"  {'thr':>4} {'nodes':>5} {'makespan_ms':>11} {'throughput':>11} {'speedup':>8} {'eff':>6} {'mem_MB/nd':>9} {'mem_MB_tot':>10} {'ok':>6}")
            for r in grp:
                print(f"  {r['threads']:>4} {str(r['nodes']):>5} {str(r['wall_ms_median']):>11} "
                      f"{str(round(r['throughput'],1) if r['throughput'] else ''):>11} "
                      f"{str(r['speedup']):>8} {str(r['efficiency']):>6} "
                      f"{str(round(r['peak_mem_mb_per_node'],1) if r['peak_mem_mb_per_node'] else ''):>9} "
                      f"{str(round(r['peak_mem_mb_total'],1) if r['peak_mem_mb_total'] else ''):>10} "
                      f"{str(r['success']):>6}")
            print()
else:
    print("no input CSVs found — run wc_scaling.sh / stream_scaling.sh first")
