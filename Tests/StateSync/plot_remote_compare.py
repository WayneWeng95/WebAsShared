#!/usr/bin/env python3
"""Remote StateSync — Redis (remote) vs Shared memory + RDMA (reference + ours).

A focused, three-series version of StateSync-remote's `latency_throughput_remote`
figure: one-way latency (left) + delivery throughput (right), same styling, with:
  - redis-remote                  baseline KV ping-pong (committed remote results)
  - Shared memory + RDMA (ref)    rdma-shm from the committed remote results
  - Shared memory + RDMA (ours)   rdma-shm we measured into ./results_remote.csv

CSV schema: approach,size_bytes,iters,lat_mean_us,lat_p50_us,lat_p99_us,gibps

  ./plot_remote_compare.py                 # -> figs/remote_redis_vs_rdma.png
  ./plot_remote_compare.py --metric p50
  ./plot_remote_compare.py --baseline-csv X --ours-csv Y
"""

import argparse
import csv
import os
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_BASELINE = os.path.join(HERE, "..", "Micro-Benchmarks", "StateSync-remote", "results.csv")
DEFAULT_OURS = os.path.join(HERE, "results_remote.csv")

# Three series in plot order.  "rdma-shm-ours" is a synthetic key we assign to the
# rdma-shm rows from OUR results CSV so they show as a distinct group.
KEEP = ["redis-remote", "cloudburst-warm", "rdma-shm", "rdma-shm-ours"]
LABEL = {
    "redis-remote":    "Redis (remote)",
    "cloudburst-warm": "Cloudburst",
    "rdma-shm":        "Shared memory + RDMA",
    "rdma-shm-ours":   "WasMem",
}
COLOR = {"redis-remote": "#e8843c", "cloudburst-warm": "#6a4c93",
         "rdma-shm": "#1d7a3e", "rdma-shm-ours": "#2057c7"}
MARKER = {"redis-remote": "^", "cloudburst-warm": "X", "rdma-shm": "*", "rdma-shm-ours": "o"}

TICK_SIZE, LABEL_SIZE, LEGEND_SIZE, YLABEL_SIZE = 16, 16, 17, 16
plt.rcParams.update({"xtick.labelsize": TICK_SIZE, "ytick.labelsize": TICK_SIZE,
                     "axes.labelsize": LABEL_SIZE, "legend.fontsize": LEGEND_SIZE})


def fmt_size(n):
    n = int(n)
    if n < 1024: return f"{n}B"
    if n < 1024 ** 2: return f"{n // 1024}KiB"
    if n < 1024 ** 3: return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


def load(baseline_csv, ours_csv):
    """Return {series: {size: row}} for the three groups.

    redis-remote + rdma-shm come from the committed baseline CSV; the rdma-shm
    rows from OUR CSV are re-keyed to 'rdma-shm-ours' so they plot separately.
    """
    data = defaultdict(dict)
    if os.path.exists(baseline_csv):
        for r in csv.DictReader(open(baseline_csv)):
            if r["approach"] in ("redis-remote", "cloudburst-warm", "rdma-shm"):
                data[r["approach"]][int(r["size_bytes"])] = r
    else:
        print(f"[plot] note: baseline {baseline_csv} not found")
    if os.path.exists(ours_csv):
        for r in csv.DictReader(open(ours_csv)):
            if r["approach"] == "rdma-shm":
                data["rdma-shm-ours"][int(r["size_bytes"])] = r
    else:
        print(f"[plot] note: ours {ours_csv} not found")
    return data


def style(a):
    return dict(color=COLOR[a], marker=MARKER[a], label=LABEL[a], linewidth=2.0,
                markersize=10 if a.startswith("rdma-shm") else 7,
                ls="--" if a == "rdma-shm-ours" else "-")


def series(data):
    return [a for a in KEEP if a in data]


def axis(ax, sizes):
    ax.set_xticks(range(len(sizes)))
    ax.set_xticklabels([fmt_size(s) for s in sizes], fontsize=TICK_SIZE)
    ax.set_xlabel("state size")
    ax.grid(True, which="both", ls=":", alpha=0.4)


def main():
    ap = argparse.ArgumentParser(description="Remote: Redis vs Shared memory + RDMA (ref + ours)")
    ap.add_argument("--baseline-csv", default=DEFAULT_BASELINE,
                    help="committed remote results (redis-remote + reference rdma-shm)")
    ap.add_argument("--ours-csv", default=DEFAULT_OURS,
                    help="our measured rdma-shm (results_remote.csv)")
    ap.add_argument("--outdir", default=os.path.join(HERE, "figs"))
    ap.add_argument("--format", default="png", choices=["png", "pdf", "svg"])
    ap.add_argument("--metric", choices=["mean", "p50"], default="mean")
    ap.add_argument("--figsize", default="9,3")
    args = ap.parse_args()

    data = load(args.baseline_csv, args.ours_csv)
    aps = series(data)
    missing = [a for a in KEEP if a not in data]
    if missing:
        print(f"[plot] note: missing series {missing}; plotting {aps}")
    if not aps:
        raise SystemExit("[plot] no redis-remote / rdma-shm rows found")

    sizes = sorted({s for a in aps for s in data[a]})
    idx = {s: i for i, s in enumerate(sizes)}
    m = args.metric
    stat = "Mean" if m == "mean" else "Median"
    figsize = tuple(float(x) for x in args.figsize.split(","))

    fig, (axL, axR) = plt.subplots(1, 2, figsize=figsize)
    for a in aps:                                  # left: one-way latency
        xs = sorted(data[a])
        axL.plot([idx[s] for s in xs], [float(data[a][s][f"lat_{m}_us"]) for s in xs], **style(a))
    axL.set_yscale("log")
    axis(axL, sizes)
    axL.set_ylabel(f"{stat} one-way latency (µs, log)", fontsize=YLABEL_SIZE)

    for a in aps:                                  # right: throughput
        xs = sorted(data[a])
        axR.plot([idx[s] for s in xs], [float(data[a][s]["gibps"]) for s in xs], **style(a))
    axR.set_yscale("log")
    axis(axR, sizes)
    axR.set_ylabel("Throughput (GiB/s, log)", fontsize=YLABEL_SIZE)

    fig.tight_layout()
    handles, labels = axL.get_legend_handles_labels()
    ncol = 2 if len(labels) >= 4 else min(len(labels), 3)
    fig.legend(handles, labels, loc="lower center", ncol=ncol,
               fontsize=LEGEND_SIZE, frameon=False, bbox_to_anchor=(0.5, 1.0))
    tag = "" if m == "mean" else f"_{m}"
    out = os.path.join(args.outdir, f"remote_redis_vs_rdma{tag}.{args.format}")
    os.makedirs(args.outdir, exist_ok=True)
    fig.savefig(out, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {out}  ({m})")

    # Speedup table: each RDMA series vs Redis (remote).
    if "redis-remote" in data:
        for rdma in ("rdma-shm", "rdma-shm-ours"):
            if rdma not in data:
                continue
            print(f"\n{stat} one-way latency: {LABEL[rdma]} speedup vs Redis (remote)")
            for s in sizes:
                if s in data[rdma] and s in data["redis-remote"]:
                    r = float(data["redis-remote"][s][f"lat_{m}_us"])
                    k = float(data[rdma][s][f"lat_{m}_us"])
                    print(f"  {fmt_size(s):>8}: {r / k:6.1f}x   (redis {r:.1f}us vs rdma {k:.1f}us)")


if __name__ == "__main__":
    main()
