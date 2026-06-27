#!/usr/bin/env python3
"""Compare the REAL-engine zero-copy state delivery against the existing
StateSync-local baselines (Redis-local and the modelled shared-memory zero-copy).

Four series, one shared 1->1 put/get test case:
  redis-local          Redis over loopback                  (from ../Micro-Benchmarks/StateSync-local/results.csv)
  shm-copy             modelled /dev/shm mmap, copy-out      (same file)
  shm-zerocopy         modelled /dev/shm mmap, memoryview    (same file)
  shm-zerocopy-engine  REAL engine page-chain Bridge splice  (this dir's results_local.csv)

Renders, into figs/:
  compare_put_get.<fmt>         combined PUT|GET latency panel vs size (log y)
  compare_get_throughput.<fmt>  GET throughput (GiB/s) vs size
and prints how many times faster the engine zero-copy delivers state.

Usage:
  ./plot_compare.py
  ./plot_compare.py --metric mean --format pdf
  ./plot_compare.py --engine-csv results_local.csv \
      --baseline-csv ../Micro-Benchmarks/StateSync-local/results.csv
"""

import argparse
import csv
import os
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.lines import Line2D
from matplotlib.ticker import FuncFormatter

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_BASELINE = os.path.join(HERE, "..", "Micro-Benchmarks", "StateSync-local", "results.csv")
DEFAULT_ENGINE = os.path.join(HERE, "results_local.csv")
DEFAULT_ROADRUNNER = os.path.join(HERE, "results_roadrunner.csv")

# The series we compare, in presentation order (slow -> fast).
SERIES = ["s3", "redis-local", "rr-embedded",
          "shm-copy", "shm-zerocopy", "shm-zerocopy-engine"]
LABEL = {
    "s3":                  "AWS Step Functions (Minio/S3)",
    "redis-local":         "Cloudburst (Redis local)",
    "rr-embedded":         "Roadrunner (Shim leverages IPC)",
    "shm-copy":            "Faasm (Shared memory copy)",
    "shm-zerocopy":        "RMMap (Shared memory zero-copy)",
    "shm-zerocopy-engine": "WasMem (Zero-copy memory routing)",
}
COLOR = {  # unified muted-wave palette (WasMem = saturated blue pop)
    "s3":                  "#c97b7b",   # muted red
    "redis-local":         "#e0be72",   # muted gold
    "rr-embedded":         "#a17a52",   # brown (Roadrunner, kept clear of RTSFaaS)
    "shm-copy":            "#6fa8a0",   # muted teal
    "shm-zerocopy":        "#86b07d",   # muted sage (zero-copy baseline)
    "shm-zerocopy-engine": "#3a5fc4",   # WasMem blue (ours, pop)
}
MARKER = {"s3": "s", "redis-local": "v", "rr-embedded": "P",
          "shm-copy": "D", "shm-zerocopy": "*", "shm-zerocopy-engine": "o"}

# Human-friendly operation names for the figures: PUT->write, GET->read.
OP_LABEL = {"put": "Write", "get": "Read"}

# Font sizes — adopted from StateSync-local/plot.py so the figures match.
TICK_SIZE   = 16
LABEL_SIZE  = 16
LEGEND_SIZE = 15
YLABEL_SIZE = 14          # y-axis labels are longer — a touch smaller than LABEL_SIZE
plt.rcParams.update({
    "xtick.labelsize": TICK_SIZE,
    "ytick.labelsize": TICK_SIZE,
    "axes.labelsize":  LABEL_SIZE,
    "legend.fontsize": LEGEND_SIZE,
})


def fmt_size(n):
    n = int(n)
    if n < 1024: return f"{n}B"
    if n < 1024 ** 2: return f"{n // 1024}KiB"
    if n < 1024 ** 3: return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


def load_rows(path, readers):
    """Return {approach: {size: row}} for rows matching `readers`, if file exists."""
    out = defaultdict(dict)
    if not os.path.exists(path):
        print(f"[plot] WARNING: {path} not found — skipping its series")
        return out
    for r in csv.DictReader(open(path)):
        if int(r.get("readers", 1)) != readers:
            continue
        out[r["approach"]][int(r["size_bytes"])] = r
    return out


def collect(baseline_csv, engine_csv, roadrunner_csv, readers):
    base = load_rows(baseline_csv, readers)
    eng = load_rows(engine_csv, readers)
    rr = load_rows(roadrunner_csv, readers)
    data = {}
    for a in ("s3", "redis-local", "shm-copy", "shm-zerocopy"):
        if a in base:
            data[a] = base[a]
    if "rr-embedded" in rr:
        data["rr-embedded"] = rr["rr-embedded"]
    if "shm-zerocopy-engine" in eng:
        data["shm-zerocopy-engine"] = eng["shm-zerocopy-engine"]
    missing = [a for a in SERIES if a not in data]
    if missing:
        print(f"[plot] note: series not found in CSVs: {missing}")
    if not data:
        raise SystemExit("[plot] no matching rows found in either CSV")
    return data


def present(data):
    return [a for a in SERIES if a in data]


def all_sizes(data):
    return sorted({s for a in data for s in data[a]})


def style(a):
    return dict(color=COLOR[a], marker=MARKER[a], label=LABEL[a],
                linewidth=1.8, markersize=8 if a == "shm-zerocopy-engine" else 6)


def line_plot(data, col, ylabel, outpath, figsize, logy=True):
    sizes = all_sizes(data)
    idx = {s: i for i, s in enumerate(sizes)}
    fig, ax = plt.subplots(figsize=figsize)
    for a in present(data):
        xs = [idx[s] for s in sorted(data[a])]
        ys = [float(data[a][s][col]) for s in sorted(data[a])]
        ax.plot(xs, ys, **style(a))
    if logy:
        ax.set_yscale("log")
    ax.set_xticks(range(len(sizes)))
    ax.set_xticklabels([fmt_size(s) for s in sizes], fontsize=TICK_SIZE)
    ax.set_xlabel("state size")
    ax.set_ylabel(ylabel, fontsize=YLABEL_SIZE)
    ax.grid(True, which="both", ls=":", alpha=0.4)
    ax.legend(fontsize=LEGEND_SIZE, frameon=False)
    fig.tight_layout()
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


# Throughput broken-axis bands (LINEAR GiB/s). The break splits the KVS/copy baselines
# (low band) from the zero-copy engines (high band); the gap 11..13 is empty.
THR_LO = (0.0, 11.0)          # s3, redis, rr-embedded, shm-copy
THR_HI = (13.0, 155000.0)     # shm-zerocopy, shm-zerocopy-engine (ours)


def panel_plot(data, metric, outpath, figsize):
    """LEFT = Mean latency (average of Read+Write, log). RIGHT = throughput (average of
    Read+Write, one line per approach) on a BROKEN LOG y-axis. One shared legend above."""
    sizes = all_sizes(data)
    idx = {s: i for i, s in enumerate(sizes)}
    stat = "Mean" if metric == "mean" else "Median"
    fig = plt.figure(figsize=figsize)
    gs = fig.add_gridspec(2, 2, width_ratios=[1, 1], height_ratios=[1, 1],
                          hspace=0.05, wspace=0.38, top=1.0, bottom=0.14)
    ax_lat = fig.add_subplot(gs[:, 0])
    ax_thi = fig.add_subplot(gs[0, 1])
    ax_tlo = fig.add_subplot(gs[1, 1], sharex=ax_thi)

    # ONE line per approach: the mean over the two directions (Read=get, Write=put).
    def draw(ax, col_suffix):
        for a in present(data):
            ss = sorted(data[a])
            xs = [idx[s] for s in ss]
            ys = [(float(data[a][s][f"get_{col_suffix}"]) +
                   float(data[a][s][f"put_{col_suffix}"])) / 2.0 for s in ss]
            st = style(a)
            st.pop("label", None)
            ax.plot(xs, ys, **st)
        ax.grid(True, which="both", ls=":", alpha=0.4)

    # LEFT — Mean latency = average of Read+Write (log)
    draw(ax_lat, f"{metric}_us")
    ax_lat.set_yscale("log")
    ax_lat.set_xticks(range(len(sizes)))
    ax_lat.set_xticklabels([fmt_size(s) for s in sizes], fontsize=TICK_SIZE)
    ax_lat.set_xlabel("state size")
    ax_lat.set_ylabel(f"{stat} latency (µs, log)", fontsize=YLABEL_SIZE)

    # RIGHT — throughput = average of Read+Write, BROKEN LINEAR y-axis
    for ax in (ax_thi, ax_tlo):
        draw(ax, "gibps")
    ax_thi.set_ylim(*THR_HI)
    ax_tlo.set_ylim(*THR_LO)
    # compact high-band ticks (50k/100k/150k) so they don't crowd the y-label
    ax_thi.set_yticks([50000, 100000, 150000])
    ax_thi.yaxis.set_major_formatter(FuncFormatter(lambda x, _: f"{x/1000:.0f}k"))
    ax_thi.spines["bottom"].set_visible(False)
    ax_tlo.spines["top"].set_visible(False)
    ax_thi.tick_params(axis="x", bottom=False, labelbottom=False)
    ax_tlo.set_xticks(range(len(sizes)))
    ax_tlo.set_xticklabels([fmt_size(s) for s in sizes], fontsize=TICK_SIZE)
    ax_tlo.set_xlabel("state size")
    d = 0.5
    bkw = dict(marker=[(-1, -d), (1, d)], markersize=8, linestyle="none",
               color="k", mec="k", mew=1, clip_on=False)
    ax_thi.plot([0, 1], [0, 0], transform=ax_thi.transAxes, **bkw)
    ax_tlo.plot([0, 1], [1, 1], transform=ax_tlo.transAxes, **bkw)
    ax_tlo.set_ylabel("Throughput (GiB/s)", fontsize=YLABEL_SIZE)
    ax_tlo.yaxis.set_label_coords(-0.18, 1.05)

    # One shared legend above the panels — one entry per approach (column-major: slow
    # baselines in col 1, fast shared-memory in col 2).
    handles = [Line2D([0], [0], color=COLOR[a], marker=MARKER[a], linewidth=1.8,
                      markersize=8 if a == "shm-zerocopy-engine" else 7, label=LABEL[a])
               for a in present(data)]
    fig.legend(handles=handles, loc="lower center", ncol=2, fontsize=LEGEND_SIZE,
               frameon=False, bbox_to_anchor=(0.5, 1.0))
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {outpath}")


def speedup(data, op, metric):
    base = "shm-zerocopy-engine"
    if base not in data:
        return
    eng = {s: float(r[f"{op}_{metric}_us"]) for s, r in data[base].items()}
    others = [a for a in present(data) if a != base]
    print(f"\n=== {OP_LABEL[op]} {metric} speedup of {LABEL[base]} vs others (x) ===")
    print(f"{'size':>8}  " + "".join(f"{LABEL[a]:>38}" for a in others))
    for s in all_sizes(data):
        cells = []
        for a in others:
            v = data[a].get(s)
            cells.append(f"{float(v[f'{op}_{metric}_us']) / eng[s]:>37.1f}x"
                         if v and eng.get(s) else f"{'-':>38}")
        print(f"{fmt_size(s):>8}  " + "".join(cells))


def main():
    ap = argparse.ArgumentParser(description="Compare engine zero-copy vs StateSync-local baselines")
    ap.add_argument("--baseline-csv", default=DEFAULT_BASELINE)
    ap.add_argument("--engine-csv", default=DEFAULT_ENGINE)
    ap.add_argument("--roadrunner-csv", default=DEFAULT_ROADRUNNER)
    ap.add_argument("--outdir", default=os.path.normpath(os.path.join(HERE, "..", "Figures")))
    ap.add_argument("--format", default="pdf", choices=["png", "pdf", "svg"])
    ap.add_argument("--readers", type=int, default=1)
    ap.add_argument("--metric", choices=["p50", "mean"], default="mean")
    ap.add_argument("--figsize", default="9,3",
                    help="combined-panel size in inches W,H (matches StateSync-local; default 9,3)")
    args = ap.parse_args()

    figsize = tuple(float(x) for x in args.figsize.split(","))
    data = collect(args.baseline_csv, args.engine_csv, args.roadrunner_csv, args.readers)
    os.makedirs(args.outdir, exist_ok=True)
    ext, m = args.format, args.metric

    tag = "" if m == "p50" else f"_{m}"   # keep p50 filenames stable; tag mean
    panel_plot(data, m, os.path.join(args.outdir, f"compare_put_get{tag}.{ext}"), figsize)
    # Single-panel throughput uses half the width so its aspect matches the panels.
    line_plot(data, "get_gibps", "Read throughput (GiB/s, log)",
              os.path.join(args.outdir, f"compare_get_throughput.{ext}"),
              (figsize[0] / 2, figsize[1]))

    speedup(data, "get", m)
    speedup(data, "put", m)


if __name__ == "__main__":
    main()
