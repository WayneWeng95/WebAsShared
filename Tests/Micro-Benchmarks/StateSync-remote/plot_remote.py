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
    "s3-disk":         "S3 (disk)",
    "s3":              "S3 (RAM)",
    "redis-remote":    "Redis (remote)",
    "cloudburst-cold": "Cloudburst (cold)",
    "cloudburst-warm": "Cloudburst (warm)",
    "rdma-shm":        "Shared memory + RDMA",
}
COLOR = {
    "s3-disk": "#8c1d40", "s3": "#d1495b", "redis-remote": "#e8843c",
    "cloudburst-cold": "#5b8fb9", "cloudburst-warm": "#6a4c93",
    "rdma-shm": "#1d7a3e",
}
MARKER = {
    "s3-disk": "o", "s3": "s", "redis-remote": "^",
    "cloudburst-cold": "D", "cloudburst-warm": "X",
    "rdma-shm": "*",
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


def approaches_in(data):
    return [a for a in ORDER if a in data] + [a for a in data if a not in ORDER]


FIELDS = ["approach", "size_bytes", "iters",
          "lat_mean_us", "lat_p50_us", "lat_p99_us", "gibps"]


def write_merged(data, path):
    """Write all approaches into one combined CSV, ordered by approach then size."""
    with open(path, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(FIELDS)
        for a in approaches_in(data):
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


def plot_panel(data, outpath, figsize, metric, yscale="log"):
    """Latency (left) + throughput (right) side by side, one shared legend on top."""
    all_sizes = sorted({s for a in data for s, _ in data[a]})
    idx = {s: i for i, s in enumerate(all_sizes)}
    stat = "Mean" if metric == "mean" else "Median"
    fig, (axL, axR) = plt.subplots(1, 2, figsize=figsize)

    for a in approaches_in(data):                      # left: latency
        xs = [idx[s] for s, _ in data[a]]
        ys = [float(r[f"lat_{metric}_us"]) for _, r in data[a]]
        axL.plot(xs, ys, **style(a))
    axL.set_yscale(yscale)
    _xaxis(axL, all_sizes)
    unit = "µs, log" if yscale == "log" else "µs"
    axL.set_ylabel(f"{stat} latency ({unit})", fontsize=YLABEL_SIZE)

    for a in approaches_in(data):                      # right: throughput
        xs = [idx[s] for s, _ in data[a]]
        ys = [float(r["gibps"]) for _, r in data[a]]
        axR.plot(xs, ys, **style(a))
    axR.set_yscale("log")
    _xaxis(axR, all_sizes)
    axR.set_ylabel("Throughput (GiB/s, log)", fontsize=YLABEL_SIZE)

    fig.tight_layout()
    handles, labels = axL.get_legend_handles_labels()
    fig.legend(handles, labels, loc="lower center", ncol=min(len(labels), 3),
               fontsize=LEGEND_SIZE, framealpha=0.9, bbox_to_anchor=(0.5, 1.0))
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
    ap.add_argument("--outdir", default="figs")
    ap.add_argument("--format", default="png", choices=["png", "pdf", "svg"])
    ap.add_argument("--figsize", default="9,3")
    ap.add_argument("--metric", choices=["mean", "p50"], default="mean")
    ap.add_argument("--yscale", choices=["linear", "log"], default="log",
                    help="y-axis for latency_remote (default log; linear emphasizes large-size blow-up)")
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
               figsize, args.metric, args.yscale)
    plot_bars(data, os.path.join(args.outdir, f"latency_remote_bars.{ext}"), figsize, args.metric)
    print_speedup(data, args.metric)


if __name__ == "__main__":
    main()
