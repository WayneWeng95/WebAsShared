#!/usr/bin/env python3
"""Plot StateSync-remote results — cross-node one-way state-transfer latency.

Reads the CSVs produced by the two harnesses (same schema):
    rdma_latency  (Rust)   -> rdma-shm
    bench_remote.py        -> redis-remote, s3-disk
CSV columns: approach,size_bytes,iters,lat_mean_us,lat_p50_us,lat_p99_us,gibps

Renders:
    latency_remote.<fmt>       one-way latency vs state size (log y)
    throughput_remote.<fmt>    delivery throughput (GiB/s) vs size
    latency_remote_bars.<fmt>  grouped bars per size
and prints a speedup table (rdma-shm vs the store-based approaches).

Usage:
    python3 plot_remote.py                      # reads *_results.csv in cwd
    python3 plot_remote.py --csv a.csv b.csv c.csv --format pdf
"""

import argparse
import csv
import glob
import os
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

# ── Font sizes (match StateSync-local) ────────────────────────────────────────
TICK_SIZE, LABEL_SIZE, LEGEND_SIZE, YLABEL_SIZE = 16, 16, 17, 16
plt.rcParams.update({
    "xtick.labelsize": TICK_SIZE, "ytick.labelsize": TICK_SIZE,
    "axes.labelsize": LABEL_SIZE, "legend.fontsize": LEGEND_SIZE,
})

ORDER = ["s3-disk", "s3", "cloudburst-cold", "cloudburst-warm", "redis-remote", "rdma-shm"]
LABEL = {
    # "s3-disk":         "Step Function (S3 disk)",
    "s3":              "AWS Step Function (Minio/S3)",
    "redis-remote":    "SONIC (Redis remote)",
    # "cloudburst-cold": "Cloudburst (Redis local)",
    "cloudburst-warm": "Cloudburst (Redis local)",
    "rdma-shm":        "RTSFaaS (RDMA shared memory)",
}
COLOR = {  # unified muted-wave palette (rdma-shm = RTSFaaS, mauve)
    "s3-disk": "#9d5a60", "s3": "#c97b7b",
    "redis-remote": "#d69a60",
    "cloudburst-cold": "#cbb78c",
    "cloudburst-warm": "#e0be72",
    "rdma-shm": "#b07ba6",   # RTSFaaS (RDMA shared memory)
}
# COLOR = {
#     "s3-disk":      "#8c1d40",   # dark red
#     "s3":           "#d1495b",   # red
#     "redis-remote": "#e8843c",   # orange
#     "redis-local":  "#edae49",   # amber
#     "shm-copy":     "#2a9d8f",   # teal
#     "shm-zerocopy": "#1d7a3e",   # green
# }
MARKER = {
    "s3-disk": "P",
    "s3": "s",
    "redis-remote": "^",
    "cloudburst-cold": "X",
    "cloudburst-warm": "v",
    "rdma-shm": "D",   # RTSFaaS
}



def fmt_size(n):
    n = int(n)
    if n < 1024: return f"{n}B"
    if n < 1024 ** 2: return f"{n // 1024}KiB"
    if n < 1024 ** 3: return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


def load(paths):
    data = defaultdict(list)
    for p in paths:
        for r in csv.DictReader(open(p)):
            data[r["approach"]].append((int(r["size_bytes"]), r))
    for a in data:
        data[a].sort(key=lambda t: t[0])
    if not data:
        raise SystemExit(f"no rows found in {paths}")
    return data


def all_approaches(data):
    """Every approach present in the data, in canonical ORDER (for archival)."""
    return [a for a in ORDER if a in data] + [a for a in data if a not in ORDER]


def approaches_in(data):
    """Approaches to DISPLAY (plots + speedup), in canonical order — only those
    with an active LABEL entry, so commenting a case out of LABEL drops it from
    every figure.  (write_merged still archives all approaches.)"""
    return [a for a in all_approaches(data) if a in LABEL]


FIELDS = ["approach", "size_bytes", "iters",
          "lat_mean_us", "lat_p50_us", "lat_p99_us", "gibps"]


def write_merged(data, path):
    """Write all approaches into one combined CSV, ordered by approach then size."""
    with open(path, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(FIELDS)
        for a in all_approaches(data):
            for _, r in data[a]:
                w.writerow([r.get(k, "") for k in FIELDS])
    print(f"[plot] merged -> {path}")


def style(a):
    return dict(color=COLOR.get(a, "#444"), marker=MARKER.get(a, "o"),
                label=LABEL.get(a, a), linewidth=1.8,
                markersize=9 if a == "rdma-shm" else 6)


def _xaxis(ax, all_sizes):
    ax.set_xticks(range(len(all_sizes)))
    ax.set_xticklabels([fmt_size(s) for s in all_sizes], fontsize=TICK_SIZE)
    ax.set_xlabel("state size")
    ax.grid(True, which="both", ls=":", alpha=0.4)


def plot_latency(data, outpath, figsize, metric, yscale="linear"):
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    idx = {s: i for i, s in enumerate(all_sizes)}
    fig, ax = plt.subplots(figsize=figsize)
    for a in approaches_in(data):
        xs = [idx[s] for s, _ in data[a]]
        ys = [float(r[f"lat_{metric}_us"]) for _, r in data[a]]
        ax.plot(xs, ys, **style(a))
    stat = "Mean" if metric == "mean" else "Median"
    ax.set_yscale(yscale)
    _xaxis(ax, all_sizes)
    unit = "µs, log" if yscale == "log" else "µs"
    ax.set_ylabel(f"{stat} one-way latency ({unit})", fontsize=YLABEL_SIZE)
    ax.legend(fontsize=LEGEND_SIZE, framealpha=0.9, loc="upper left")
    fig.tight_layout()
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


def plot_throughput(data, outpath, figsize):
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    idx = {s: i for i, s in enumerate(all_sizes)}
    fig, ax = plt.subplots(figsize=figsize)
    for a in approaches_in(data):
        xs = [idx[s] for s, _ in data[a]]
        ys = [float(r["gibps"]) for _, r in data[a]]
        ax.plot(xs, ys, **style(a))
    ax.set_yscale("log")
    _xaxis(ax, all_sizes)
    ax.set_ylabel("Throughput (GiB/s, log)", fontsize=YLABEL_SIZE)
    ax.legend(fontsize=LEGEND_SIZE, framealpha=0.9)
    fig.tight_layout()
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


# Approach whose throughput sits in the upper band of the broken axis (the one
# that dwarfs the store-based rows).  The lower band holds everyone else.
HIGH_BAND = "rdma-shm"

# This cluster's per-port wire ceiling: ConnectX-3 Pro at 10 GbE (ethtool tops
# out at 10000baseKR — no 40 G mode).  10 Gb/s / 8 / 1024^3 ≈ 1.16 GiB/s raw
# line rate — the hardware bandwidth limit a single link can approach.
PORT_CEILING_GIBPS = 10e9 / 8 / (1024 ** 3)


def plot_panel(data, outpath, figsize, metric, yscale="log", ceiling=PORT_CEILING_GIBPS):
    """Latency (left) + throughput (right) side by side, one shared legend on top.

    The throughput axis is LINEAR with a *broken* y-axis: an upper band for the
    high-bandwidth approach (`HIGH_BAND`) and a lower band — on its own expanded
    scale — for the store-based rows, so every series is readable at its true
    GiB/s instead of being compressed by a log axis.  Falls back to a single
    linear axis if the two groups don't separate cleanly.
    """
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    idx = {s: i for i, s in enumerate(all_sizes)}
    stat = "Mean" if metric == "mean" else "Median"
    disp = approaches_in(data)

    def gibps(a):
        return [float(r["gibps"]) for _, r in data[a]]

    def xs_of(a):
        return [idx[s] for s, _ in data[a]]

    high_vals = [v for a in disp if a == HIGH_BAND for v in gibps(a)]
    low_vals  = [v for a in disp if a != HIGH_BAND for v in gibps(a)]
    # Only break the axis when the high series sits clearly above the rest.
    can_break = bool(high_vals) and bool(low_vals) and min(high_vals) > max(low_vals) * 1.1

    fig = plt.figure(figsize=figsize)
    gs = fig.add_gridspec(2, 2, height_ratios=[1.0, 1.25], hspace=0.12, wspace=0.34)

    # ── Left: latency (single axis, spans both rows) ──────────────────────────
    axL = fig.add_subplot(gs[:, 0])
    for a in disp:
        axL.plot(xs_of(a), [float(r[f"lat_{metric}_us"]) for _, r in data[a]], **style(a))
    axL.set_yscale(yscale)
    _xaxis(axL, all_sizes)
    unit = "µs, log" if yscale == "log" else "µs"
    axL.set_ylabel(f"{stat} latency ({unit})", fontsize=YLABEL_SIZE)

    # ── Right: throughput (broken linear axis) ────────────────────────────────
    if can_break:
        axT = fig.add_subplot(gs[0, 1])                 # upper band: HIGH_BAND
        axB = fig.add_subplot(gs[1, 1], sharex=axT)     # lower band: store rows
        for ax in (axT, axB):
            for a in disp:
                ax.plot(xs_of(a), gibps(a), **style(a))
            ax.grid(True, which="both", ls=":", alpha=0.4)
        axB.set_ylim(0, max(low_vals) * 1.25)
        top = max(high_vals) * 1.08
        if ceiling and ceiling > 0:
            top = max(top, ceiling * 1.16)             # headroom above the line
        axT.set_ylim(min(high_vals) * 0.85, top)

        # Machine bandwidth ceiling (10 GbE line rate).  Label sits just UNDER
        # the dashed line, in the empty gap above the highest series, so it never
        # collides with the top spine or the data points.
        if ceiling and ceiling > 0:
            axT.axhline(ceiling, ls="--", color="#777", lw=1.4)
            axT.text(0.02, ceiling * 0.98, f"10 GbE line rate (~{ceiling:.2f} GiB/s)",
                     color="black", fontsize=13, va="top", ha="left",
                     transform=axT.get_yaxis_transform())

        # Hide the facing spines / ticks and draw the diagonal break marks.
        axT.spines["bottom"].set_visible(False)
        axB.spines["top"].set_visible(False)
        axT.tick_params(axis="x", which="both", bottom=False, labelbottom=False)
        axB.set_xticks(range(len(all_sizes)))
        axB.set_xticklabels([fmt_size(s) for s in all_sizes], fontsize=TICK_SIZE)
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
        for a in disp:
            axR.plot(xs_of(a), gibps(a), **style(a))
        _xaxis(axR, all_sizes)
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
    fig.legend(handles, labels, loc="lower center", ncol=2,
               fontsize=LEGEND_SIZE, frameon=False, bbox_to_anchor=(0.5, 1.0))
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


def plot_bars(data, outpath, figsize, metric):
    aps = approaches_in(data)
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    n = len(aps)
    width = 0.8 / n
    fig, ax = plt.subplots(figsize=figsize)
    for i, a in enumerate(aps):
        lut = {s: float(r[f"lat_{metric}_us"]) for s, r in data[a]}
        xs = [j + (i - n / 2 + 0.5) * width for j in range(len(all_sizes))]
        ax.bar(xs, [lut.get(s, 0) for s in all_sizes], width=width,
               color=COLOR.get(a, "#444"), label=LABEL.get(a, a))
    stat = "Mean" if metric == "mean" else "Median"
    ax.set_yscale("log")
    _xaxis(ax, all_sizes)
    ax.set_ylabel(f"{stat} latency (µs, log)", fontsize=YLABEL_SIZE)
    ax.legend(fontsize=LEGEND_SIZE, framealpha=0.9)
    fig.tight_layout()
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


def print_speedup(data, metric, baseline="rdma-shm"):
    if baseline not in data:
        return
    base = {s: float(r[f"lat_{metric}_us"]) for s, r in data[baseline]}
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    others = [a for a in approaches_in(data) if a != baseline]
    print(f"\n=== {metric} one-way latency speedup of {LABEL[baseline]} vs others (×) ===")
    print(f"{'size':>8}  " + "".join(f"{LABEL[a]:>30}" for a in others))
    for s in all_sizes:
        cells = []
        for a in others:
            lut = {sz: float(r[f"lat_{metric}_us"]) for sz, r in data[a]}
            cells.append(f"{lut[s] / base[s]:>29.1f}×" if s in lut and base.get(s) else f"{'-':>30}")
        print(f"{fmt_size(s):>8}  " + "".join(cells))


def main():
    ap = argparse.ArgumentParser(description="Plot StateSync-remote results")
    ap.add_argument("--csv", nargs="+", default=None,
                    help="result CSVs (default: *_results.csv in cwd)")
    ap.add_argument("--outdir", default=os.path.normpath(os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", "Figures")))
    ap.add_argument("--format", default="pdf", choices=["png", "pdf", "svg"])
    ap.add_argument("--figsize", default="9,3")
    ap.add_argument("--metric", choices=["mean", "p50"], default="mean")
    ap.add_argument("--yscale", choices=["linear", "log"], default="log",
                    help="y-axis for latency_remote (default log; linear emphasizes large-size blow-up)")
    ap.add_argument("--ceiling-gibps", type=float, default=PORT_CEILING_GIBPS,
                    help="machine bandwidth ceiling drawn on the throughput panel "
                         "(default 10 GbE line rate ~1.16 GiB/s; 0 to hide)")
    ap.add_argument("--merged-csv", default="results.csv",
                    help="combined CSV to (re)write from the inputs; '' to skip")
    args = ap.parse_args()

    if args.csv:
        paths = args.csv
    elif os.path.exists("results.csv"):
        paths = ["results.csv"]                       # single shared file (default)
    else:
        paths = sorted(glob.glob("*_results.csv"))    # back-compat: fold separate files
    if not paths:
        raise SystemExit("no results.csv and no *_results.csv found — run the benchmarks first")
    figsize = tuple(float(x) for x in args.figsize.replace("x", ",").split(","))

    data = load(paths)

    # Always emit one combined CSV (unless the only input IS that file).
    if args.merged_csv and paths != [args.merged_csv]:
        write_merged(data, args.merged_csv)

    os.makedirs(args.outdir, exist_ok=True)
    ext = args.format
    plot_panel(data, os.path.join(args.outdir, f"latency_throughput_remote.{ext}"),
               figsize, args.metric, args.yscale, args.ceiling_gibps)
    plot_bars(data, os.path.join(args.outdir, f"latency_remote_bars.{ext}"), figsize, args.metric)
    print_speedup(data, args.metric)


if __name__ == "__main__":
    main()
