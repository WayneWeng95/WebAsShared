#!/usr/bin/env python3
"""plot_bars_grid.py — merge the five per-workload *_bars.pdf into ONE 3x5 grid.

Each per-workload bars figure shows, per input size, the four systems' best
end-to-end latency (s). This reproduces them from the same data/logic into a
single figure: COLUMNS = workloads (a..e), ROWS = the 3 input sizes
(small/mid/large). The workload notation (a. WordCount, ...) sits at the bottom
of each column.

Bars per panel (legend order): Cloudburst, RMMap, Faasm, WasMem (ours = AOT).
Output: figs/all_workloads_bars.pdf
"""
import csv
import os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import Patch

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FIGS = os.path.join(HERE, "figs")
os.makedirs(FIGS, exist_ok=True)

STYLE = {
    "ours":       dict(color="#2057c7", label="WasMem"),
    "faasm":      dict(color="#2a9d8f", label="Faasm"),
    "rmmap":      dict(color="#1d7a3e", label="RMMap"),
    "cloudburst": dict(color="#edae49", label="Cloudburst"),
}
ORDER = ["cloudburst", "rmmap", "faasm", "ours"]   # bar/legend order

# Font sizes — matched to the per-workload plot.py scripts in this folder.
TICK_SIZE = 16
LABEL_SIZE = 16
LEGEND_SIZE = 17
YLABEL_SIZE = 16
VALUE_SIZE = 11   # bar value labels (bars_fig convention)

# Per-workload config: display name, size column, [(size, label)], tolerance,
# and the latency column for each system (ms; converted to s).
WORKLOADS = [
    dict(name="Wordcount", dir="WordCount", scol="size_mb",
         sizes=[(50, "50 MB"), (500, "500 MB"), (1000, "1 GB")], tol=1.5,
         lat={"ours": "compute_ms", "faasm": "e2e_ms",
              "rmmap": "e2e_ms", "cloudburst": "e2e_ms_median"}),
    dict(name="Finra", dir="Finra", scol="size_trades",
         sizes=[(10000, "10k"), (100000, "100k"), (1000000, "1M")], tol=0.5,
         lat={"ours": "compute_ms", "faasm": "e2e_ms",
              "rmmap": "e2e_ms", "cloudburst": "e2e_ms"}),
    dict(name="Matrix", dir="Matrix", scol="size_n",
         sizes=[(512, "512 × 512"), (1024, "1024 × 1024"),
                (2048, "2048 × 2048")], tol=1.5,
         lat={"ours": "compute_ms", "faasm": "e2e_ms",
              "rmmap": "e2e_ms", "cloudburst": "e2e_ms_median"}),
    dict(name="ML training", dir="ML_training", scol="size_mb",
         sizes=[(3.2, "100k"), (9.7, "300k"), (19.5, "600k")], tol=0.1,
         lat={k: "compute_ms" for k in ORDER}),
    dict(name="ML inference", dir="ML_inference", scol="size_mb",
         sizes=[(3.2, "100k"), (9.7, "300k"), (19.5, "600k")], tol=0.1,
         lat={k: "compute_ms" for k in ORDER}),
]

PATHS = {
    "ours":       "results_aot.csv",
    "cloudburst": os.path.join("baseline", "cloudburst", "results.csv"),
    "rmmap":      os.path.join("baseline", "rmmap", "results.csv"),
    "faasm":      os.path.join("baseline", "faasm", "demo", "results.csv"),
}


def fnum(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None


def load(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [dict(r) for r in csv.DictReader(f)]


def best_latency_s(rows, scol, size, tol, latkey):
    """MIN latency (s) over the sweep for one input size, or None."""
    vs = [fnum(r.get(latkey)) for r in rows
          if fnum(r.get(scol)) is not None
          and abs(fnum(r[scol]) - size) <= tol and fnum(r.get(latkey))]
    return min(vs) / 1000.0 if vs else None


def fmt(y):
    return ("%.2f" % y) if y < 10 else "%.0f" % y


def main():
    nrows, ncols = 3, len(WORKLOADS)
    fig, axes = plt.subplots(nrows, ncols, figsize=(18, 6))
    letters = "abcdefghij"

    # preload rows per workload/system
    rows_by = {}
    for wl in WORKLOADS:
        wdir = os.path.join(ROOT, wl["dir"])
        rows_by[wl["name"]] = {k: load(os.path.join(wdir, PATHS[k])) for k in ORDER}

    for c, wl in enumerate(WORKLOADS):
        for rrow in range(nrows):
            ax = axes[rrow][c]
            size, slabel = wl["sizes"][rrow]
            ys = []
            for k in ORDER:
                ys.append(best_latency_s(rows_by[wl["name"]][k], wl["scol"],
                                         size, wl["tol"], wl["lat"][k]) or 0.0)
            bars = ax.bar(range(len(ORDER)), ys,
                          color=[STYLE[k]["color"] for k in ORDER],
                          edgecolor="black", linewidth=0.3)
            for b, y in zip(bars, ys):
                if y:
                    ax.text(b.get_x() + b.get_width() / 2, y, fmt(y),
                            ha="center", va="bottom", fontsize=VALUE_SIZE,
                            fontweight="bold")
            ax.set_xticks([])
            ax.margins(y=0.12)
            ax.tick_params(axis="y", labelsize=TICK_SIZE)
            ax.grid(True, axis="y", alpha=.3)
            ax.set_xlabel(slabel, fontsize=LABEL_SIZE)   # size label under every panel
            if c == 0:
                ax.set_ylabel("Latency (s)", fontsize=YLABEL_SIZE)
            # workload notation at the BOTTOM of each column
            if rrow == nrows - 1:
                ax.text(0.5, -0.28, "%s. %s" % (letters[c], wl["name"]),
                        transform=ax.transAxes, ha="center", va="top",
                        fontsize=LABEL_SIZE, fontweight="bold")

    fig.tight_layout(rect=[0, 0, 1, 0.95], w_pad=0.3, h_pad=0.5)
    fig.subplots_adjust(wspace=0.22, hspace=0.28)
    fig.align_ylabels(axes[:, 0])   # align the three "Latency (s)" labels
    handles = [Patch(color=STYLE[k]["color"], label=STYLE[k]["label"]) for k in ORDER]
    fig.legend(handles, [STYLE[k]["label"] for k in ORDER], loc="upper center",
               ncol=4, frameon=False, fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.0),
               columnspacing=2.0, handletextpad=0.6)
    out = os.path.join(FIGS, "all_workloads_bars.pdf")
    fig.savefig(out, bbox_inches="tight")
    print("wrote", out)


if __name__ == "__main__":
    main()
