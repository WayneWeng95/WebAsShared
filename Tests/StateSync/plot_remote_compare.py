#!/usr/bin/env python3
"""Remote StateSync — Redis (remote) vs Shared memory + RDMA (reference + ours).

A focused, multi-series version of StateSync-remote's `latency_throughput_remote`
figure: one-way latency (left, log) + delivery throughput (right, broken linear
axis with a 10 GbE line-rate ceiling — same panel treatment as plot_remote.py),
with:
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
    "redis-remote":    "SONIC (Redis remote)",
    "cloudburst-warm": "Cloudburst (Redis local)",
    "rdma-shm":        "RTSFaaS (Shared memory + RDMA)",
    "rdma-shm-ours":   "WasMem (Shared memory + RDMA)",
}
COLOR = {"redis-remote": "#d69a60", "cloudburst-warm": "#e0be72",
         "rdma-shm": "#b07ba6",        # RTSFaaS (shm + RDMA)
         "rdma-shm-ours": "#3a5fc4"}   # WasMem (ours, pop)
MARKER = {"redis-remote": "^", "cloudburst-warm": "v", "rdma-shm": "D", "rdma-shm-ours": "o"}

# Series that sit in the upper band of the broken throughput axis (the RDMA
# shared-memory rows that dwarf the store-based approaches).  Everyone else
# lands in the lower band, on its own expanded scale.
def is_high(a):
    return a.startswith("rdma-shm")

# This cluster's per-port wire ceiling: ConnectX-3 Pro at 10 GbE.
# 10 Gb/s / 8 / 1024^3 ≈ 1.16 GiB/s raw line rate — the hardware limit a single
# link can approach.  (Matches StateSync-remote/plot_remote.py.)
PORT_CEILING_GIBPS = 10e9 / 8 / (1024 ** 3)

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
    ap.add_argument("--outdir", default=os.path.normpath(os.path.join(HERE, "..", "Figures")))
    ap.add_argument("--format", default="pdf", choices=["png", "pdf", "svg"])
    ap.add_argument("--metric", choices=["mean", "p50"], default="mean")
    ap.add_argument("--figsize", default="9,3")
    ap.add_argument("--ceiling-gibps", type=float, default=PORT_CEILING_GIBPS,
                    help="machine bandwidth ceiling drawn on the throughput panel "
                         "(default 10 GbE line rate ~1.16 GiB/s; 0 to hide)")
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
    figsize = tuple(float(x) for x in args.figsize.replace("x", ",").split(","))
    ceiling = args.ceiling_gibps

    def xs_of(a):
        return [idx[s] for s in sorted(data[a])]

    def gibps(a):
        return [float(data[a][s]["gibps"]) for s in sorted(data[a])]

    high = [a for a in aps if is_high(a)]          # RDMA shared-memory (ref + ours)
    low  = [a for a in aps if not is_high(a)]       # store-based (Redis, Cloudburst)
    high_vals = [v for a in high for v in gibps(a)]
    low_vals  = [v for a in low for v in gibps(a)]
    # Only break the axis when the high band sits clearly above the rest.
    can_break = bool(high_vals) and bool(low_vals) and min(high_vals) > max(low_vals) * 1.1

    fig = plt.figure(figsize=figsize)
    gs = fig.add_gridspec(2, 2, height_ratios=[1.0, 1.25], hspace=0.12, wspace=0.34)

    # ── Left: one-way latency (single axis, spans both rows) ──────────────────
    axL = fig.add_subplot(gs[:, 0])
    for a in aps:
        axL.plot(xs_of(a), [float(data[a][s][f"lat_{m}_us"]) for s in sorted(data[a])], **style(a))
    axL.set_yscale("log")
    axis(axL, sizes)
    axL.set_ylabel(f"{stat} latency (µs, log)", fontsize=YLABEL_SIZE)

    # ── Right: delivery throughput (broken linear axis when bands separate) ───
    if can_break:
        axT = fig.add_subplot(gs[0, 1])             # upper band: RDMA shared mem
        axB = fig.add_subplot(gs[1, 1], sharex=axT) # lower band: store-based
        for ax in (axT, axB):
            for a in aps:
                ax.plot(xs_of(a), gibps(a), **style(a))
            ax.grid(True, which="both", ls=":", alpha=0.4)
        axB.set_ylim(0, max(low_vals) * 1.25)
        top = max(high_vals) * 1.08
        if ceiling and ceiling > 0:
            top = max(top, ceiling * 1.16)          # headroom above the line
        axT.set_ylim(min(high_vals) * 0.85, top)

        # Machine bandwidth ceiling (10 GbE line rate), label just under the line.
        if ceiling and ceiling > 0:
            axT.axhline(ceiling, ls="--", color="#777", lw=1.4)
            axT.text(0.02, ceiling * 0.98, f"10 GbE line rate (~{ceiling:.2f} GiB/s)",
                     color="black", fontsize=13, va="top", ha="left",
                     transform=axT.get_yaxis_transform())

        # Hide the facing spines / ticks and draw the diagonal break marks.
        axT.spines["bottom"].set_visible(False)
        axB.spines["top"].set_visible(False)
        axT.tick_params(axis="x", which="both", bottom=False, labelbottom=False)
        axB.set_xticks(range(len(sizes)))
        axB.set_xticklabels([fmt_size(s) for s in sizes], fontsize=TICK_SIZE)
        axB.set_xlabel("state size")
        d = 0.015
        kw = dict(transform=axT.transAxes, color="k", clip_on=False, lw=1)
        axT.plot((-d, +d), (-d, +d), **kw); axT.plot((1 - d, 1 + d), (-d, +d), **kw)
        kw.update(transform=axB.transAxes)
        axB.plot((-d, +d), (1 - d, 1 + d), **kw); axB.plot((1 - d, 1 + d), (1 - d, 1 + d), **kw)

        # One y-label, centred on the break between the two bands.
        axB.set_ylabel("Throughput (GiB/s)", fontsize=YLABEL_SIZE)
        axB.yaxis.set_label_coords(-0.16, 1.04)
    else:
        axR = fig.add_subplot(gs[:, 1])
        for a in aps:
            axR.plot(xs_of(a), gibps(a), **style(a))
        axis(axR, sizes)
        axR.set_ylabel("Throughput (GiB/s)", fontsize=YLABEL_SIZE)
        if ceiling and ceiling > 0:
            lo, hi = axR.get_ylim()
            axR.set_ylim(lo, max(hi, ceiling * 1.16))
            axR.axhline(ceiling, ls="--", color="#777", lw=1.4)
            axR.text(0.02, ceiling * 0.98, f"10 GbE line rate (~{ceiling:.2f} GiB/s)",
                     color="black", fontsize=13, va="top", ha="left",
                     transform=axR.get_yaxis_transform())

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
