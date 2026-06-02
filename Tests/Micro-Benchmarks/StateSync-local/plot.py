#!/usr/bin/env python3
"""Plot StateSync micro-benchmark results.

Reads the CSV written by bench.py (`--csv`) and renders paper-ready figures
comparing the state-synchronization approaches:

    latency_get.<fmt>     GET (consumer-side) p50 latency vs state size, log-log,
                          with a p50->p99 shaded band
    latency_put.<fmt>     PUT (producer-side) p50 latency vs state size
    throughput_get.<fmt>  GET delivery throughput (GiB/s) vs state size
    latency_get_bars.<fmt> grouped bars of GET p50 latency per size (log y)

It also prints a speedup table: how many times faster SHM zero-copy delivers
state (GET p50) than every other approach, per size.

Usage:
    python3 plot.py                       # results.csv -> figs/*.png
    python3 plot.py --csv results.csv --outdir figs --format pdf
    python3 plot.py --readers 1           # filter to one fan-out degree
"""

import argparse
import csv
import os
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")            # headless: write files, no display
import matplotlib.pyplot as plt

# ── Font sizes ────────────────────────────────────────────────────────────────
TICK_SIZE   = 16
LABEL_SIZE  = 16
LEGEND_SIZE = 17
YLABEL_SIZE = 14          # y-axis labels are longer — a touch smaller than LABEL_SIZE
plt.rcParams.update({
    "xtick.labelsize": TICK_SIZE,
    "ytick.labelsize": TICK_SIZE,
    "axes.labelsize":  LABEL_SIZE,
    "legend.fontsize": LEGEND_SIZE,
})

# ── Approach presentation order + styling (gradient: remote -> local -> ours) ──
ORDER = ["s3-disk", "s3", "redis-remote", "redis-local", "shm-copy", "shm-zerocopy"]
LABEL = {
    "s3-disk":      "S3 (disk)",
    "s3":           "S3 (RAM)",
    "redis-remote": "Redis (remote)",
    "redis-local":  "Redis (local)",
    "shm-copy":     "Shared memory (copy)",
    "shm-zerocopy": "Shared memory (zero-copy)",
}
COLOR = {
    "s3-disk":      "#8c1d40",   # dark red
    "s3":           "#d1495b",   # red
    "redis-remote": "#e8843c",   # orange
    "redis-local":  "#edae49",   # amber
    "shm-copy":     "#2a9d8f",   # teal
    "shm-zerocopy": "#1d7a3e",   # green (ours)
}
MARKER = {
    "s3-disk": "o", "s3": "s", "redis-remote": "^",
    "redis-local": "v", "shm-copy": "D", "shm-zerocopy": "*",
}


def fmt_size(n):
    n = int(n)
    if n < 1024:
        return f"{n}B"
    if n < 1024 ** 2:
        return f"{n // 1024}KiB"
    if n < 1024 ** 3:
        return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


def fmt_latency(us):
    """Human latency label from microseconds."""
    if us < 1e3:
        return f"{us:.0f} µs"
    if us < 1e6:
        return f"{us / 1e3:.0f} ms"
    return f"{us / 1e6:.2f} s"


def load(path, readers_filter):
    """Return {approach: [(size, row_dict), ...]} sorted by size."""
    rows = list(csv.DictReader(open(path)))
    if not rows:
        raise SystemExit(f"no rows in {path}")
    readers_vals = sorted({int(r["readers"]) for r in rows})
    readers = readers_filter if readers_filter is not None else readers_vals[0]
    if len(readers_vals) > 1:
        print(f"[plot] CSV has readers={readers_vals}; plotting readers={readers} "
              f"(use --readers to change)")
    data = defaultdict(list)
    for r in rows:
        if int(r["readers"]) != readers:
            continue
        data[r["approach"]].append((int(r["size_bytes"]), r))
    for a in data:
        data[a].sort(key=lambda t: t[0])
    return data, readers


def approaches_in(data):
    """ORDER first, then any extra approaches present in the CSV."""
    return [a for a in ORDER if a in data] + [a for a in data if a not in ORDER]


def style(a):
    return dict(color=COLOR.get(a, "#444"), marker=MARKER.get(a, "o"),
                label=LABEL.get(a, a), linewidth=1.8, markersize=8 if a == "shm-zerocopy" else 6)


# ── Combined PUT|GET latency panel, one shared legend, no titles ──────────────
# X is categorical (equal spacing between size groups); Y is log.
def plot_latency_panel(data, outpath, figsize, metric="mean"):
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    idx = {s: i for i, s in enumerate(all_sizes)}
    fig, axes = plt.subplots(1, 2, figsize=figsize)
    for ax, op in zip(axes, ("put", "get")):
        for a in approaches_in(data):
            xs = [idx[s] for s, _ in data[a]]
            val = [float(r[f"{op}_{metric}_us"]) for _, r in data[a]]
            ax.plot(xs, val, **style(a))
        stat = "Mean" if metric == "mean" else "Median"
        ax.set_yscale("log")
        ax.set_xticks(range(len(all_sizes)))
        ax.set_xticklabels([fmt_size(s) for s in all_sizes], fontsize=TICK_SIZE)
        ax.set_xlabel("state size")
        ax.set_ylabel(f"{stat} {op.upper()} latency (µs, log)", fontsize=YLABEL_SIZE)
        ax.grid(True, which="both", ls=":", alpha=0.4)
    # Lay out the panels, then place one shared legend entirely ABOVE them
    # (its bottom edge anchored at the figure top) so it can't overlap the axes.
    # bbox_inches="tight" expands the saved canvas to include it.
    fig.tight_layout()
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc="lower center", ncol=3, fontsize=LEGEND_SIZE,
               framealpha=0.9, bbox_to_anchor=(0.5, 1.0))
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


# ── Line plot: throughput vs size (equal-spaced x, log y) ─────────────────────
def plot_throughput(data, op, outpath, figsize):
    fig, ax = plt.subplots(figsize=figsize)
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    idx = {s: i for i, s in enumerate(all_sizes)}
    for a in approaches_in(data):
        xs = [idx[s] for s, _ in data[a]]
        gibps = [float(r[f"{op}_gibps"]) for _, r in data[a]]
        ax.plot(xs, gibps, **style(a))
    ax.set_yscale("log")
    ax.set_xticks(range(len(all_sizes)))
    ax.set_xticklabels([fmt_size(s) for s in all_sizes], fontsize=TICK_SIZE)
    ax.set_xlabel("state size")
    ax.set_ylabel(f"{op.upper()} throughput (GiB/s, log)", fontsize=YLABEL_SIZE)
    ax.grid(True, which="both", ls=":", alpha=0.4)
    ax.legend(fontsize=LEGEND_SIZE, framealpha=0.9)
    fig.tight_layout()
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


# ── Grouped bars: GET p50 per size ────────────────────────────────────────────
def plot_bars(data, op, outpath, figsize, metric="mean"):
    aps = approaches_in(data)
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    n = len(aps)
    width = 0.8 / n
    fig, ax = plt.subplots(figsize=figsize)
    for i, a in enumerate(aps):
        lut = {s: float(r[f"{op}_{metric}_us"]) for s, r in data[a]}
        xs = [j + (i - n / 2 + 0.5) * width for j in range(len(all_sizes))]
        ys = [lut.get(s, 0) for s in all_sizes]
        ax.bar(xs, ys, width=width, color=COLOR.get(a, "#444"), label=LABEL.get(a, a))
    ax.set_yscale("log")
    ax.set_xticks(range(len(all_sizes)))
    ax.set_xticklabels([fmt_size(s) for s in all_sizes], fontsize=TICK_SIZE)
    ax.set_xlabel("state size")
    stat = "Mean" if metric == "mean" else "Median"
    ax.set_ylabel(f"{stat} {op.upper()} latency (µs, log)", fontsize=YLABEL_SIZE)
    ax.grid(True, axis="y", which="both", ls=":", alpha=0.4)
    ax.legend(fontsize=LEGEND_SIZE, ncol=2, framealpha=0.9)
    fig.tight_layout()
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


# ── Speedup table (stdout) ────────────────────────────────────────────────────
def print_speedup(data, op, metric="mean", baseline="shm-zerocopy"):
    if baseline not in data:
        return
    base = {s: float(r[f"{op}_{metric}_us"]) for s, r in data[baseline]}
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    others = [a for a in approaches_in(data) if a != baseline]
    print(f"\n=== {op.upper()} {metric} speedup of {LABEL[baseline]} vs others (×) ===")
    hdr = f"{'size':>8}  " + "".join(f"{LABEL[a]:>22}" for a in others)
    print(hdr)
    for s in all_sizes:
        cells = []
        for a in others:
            lut = {sz: float(r[f"{op}_{metric}_us"]) for sz, r in data[a]}
            cells.append(f"{lut[s] / base[s]:>21.1f}×" if s in lut and base.get(s) else f"{'-':>22}")
        print(f"{fmt_size(s):>8}  " + "".join(cells))


def main():
    ap = argparse.ArgumentParser(description="Plot StateSync benchmark results")
    ap.add_argument("--csv", default="results.csv")
    ap.add_argument("--outdir", default="figs")
    ap.add_argument("--format", default="png", choices=["png", "pdf", "svg"])
    ap.add_argument("--readers", type=int, default=None,
                    help="which fan-out degree to plot (default: lowest in CSV)")
    ap.add_argument("--figsize", default="9,3",
                    help="figure size in inches as W,H (default: 9,4.5)")
    ap.add_argument("--metric", choices=["mean", "p50"], default="mean",
                    help="central latency statistic to plot (default: mean)")
    args = ap.parse_args()

    try:
        figsize = tuple(float(x) for x in args.figsize.replace("x", ",").split(","))
        assert len(figsize) == 2
    except Exception:
        raise SystemExit(f"--figsize must be 'W,H' (got {args.figsize!r})")

    data, readers = load(args.csv, args.readers)
    os.makedirs(args.outdir, exist_ok=True)
    rtag = f"_r{readers}" if readers != 1 else ""
    ext = args.format

    plot_latency_panel(data, os.path.join(args.outdir, f"latency_put_get{rtag}.{ext}"),
                       figsize, args.metric)
    plot_throughput(data, "get", os.path.join(args.outdir, f"throughput_get{rtag}.{ext}"),
                    figsize)
    plot_bars(data, "get", os.path.join(args.outdir, f"latency_get_bars{rtag}.{ext}"),
              figsize, args.metric)

    print_speedup(data, "get", args.metric)
    print_speedup(data, "put", args.metric)


if __name__ == "__main__":
    main()
