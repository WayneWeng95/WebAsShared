#!/usr/bin/env python3
"""plot_inter_bars.py — Inter-node app benchmark: Faasm vs WasMem, all workloads.

ONE figure, six workload groups along the x-axis. Each group has two two-tone
bars:

  Faasm  (muted teal)  |  WasMem / ours (saturated blue, pop)

Per bar, the FULL height is the total execution time (Σ busy node-seconds,
lighter tint of the system colour) and the saturated base inside it is the
makespan (end-to-end latency). An error bar (makespan_std) sits on the makespan
base; the total-exec value is printed on top, the makespan value inside the base.

The total-exec spread is ~12s..550s with a big empty gap (everything <= 52s
except Faasm WordCount=550s and Matrix=383s), so instead of a log axis we use a
two-level broken (split) linear y-axis: a lower band (0..70s) for the detail and
an upper band (350..600s) for the two tall Faasm bars.

Reads ../wasmem/results_<wl>.csv and ../faasm/results_<wl>.csv, picks the
LARGEST input size per workload (max reps on ties).
Output: figs/inter_node_bars.pdf  (also copied to Tests/Figures/)
"""
import argparse
import csv
import os
import shutil
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
from matplotlib.patches import Patch

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)                      # Inter-Node Application_Benchmark
TESTS = os.path.dirname(ROOT)                     # Tests/
FIGS = os.path.join(HERE, "figs")
FIGURES = os.path.join(TESTS, "Figures")
os.makedirs(FIGS, exist_ok=True)

# unified palette (PLOT_COLOR_SCHEMES.md): WasMem = saturated blue pop, Faasm = teal
SYS = {
    "faasm":  dict(color="#6fa8a0", label="Faasm"),   # muted teal
    "wasmem": dict(color="#3a5fc4", label="WasMem"),  # saturated blue (ours, pop)
}
ORDER = ["faasm", "wasmem"]   # bar order within each group

# Per-workload config: display name + size column + the largest-load size label
# shown under the group (like mem_largest_load.pdf).
# (Add the remaining comparable workloads here later — one dict each.)
WORKLOADS = [
    dict(name="WordCount",    file="wordcount",    scol="size_mb",   size="4 GB"),
    dict(name="TeraSort",     file="terasort",     scol="size_mb",   size="1.2 GB"),
    dict(name="Finra",        file="finra",        scol="trades",    size="5M"),
    dict(name="Matrix",       file="matrix",       scol="matrix_n",  size="4096 × 4096"),
    dict(name="ML training",  file="ml_training",  scol="samples",   size="6M"),
    dict(name="ML inference", file="ml_inference", scol="samples",   size="6M"),
]

# Two-level broken y-axis (seconds). Lower band holds all the detail; upper band
# holds the two tall Faasm total-exec bars (WordCount 550, Matrix 383).
LOW_TOP = 70.0
HIGH_BOT, HIGH_TOP = 350.0, 600.0

# font sizes matched to the intra-node bar grid (plot_bars_grid.py)
TICK_SIZE = 16
LABEL_SIZE = 16
LEGEND_SIZE = 17
VALUE_SIZE = 12
YLABEL_SIZE = 22


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


def pick_largest(rows, scol):
    """Row at the LARGEST input size; on ties pick the most-replicated run."""
    cand = [r for r in rows if fnum(r.get(scol)) is not None]
    if not cand:
        return None
    top = max(fnum(r[scol]) for r in cand)
    same = [r for r in cand if fnum(r[scol]) == top]
    return max(same, key=lambda r: fnum(r.get("reps")) or 0)


def lighten(hexc, f=0.62):
    """Pale tint of a hex colour (toward white) for the total-exec envelope."""
    r, g, b = (int(hexc[i:i + 2], 16) for i in (1, 3, 5))
    return "#%02x%02x%02x" % tuple(int(c + (255 - c) * f) for c in (r, g, b))


def fmt(y):
    return ("%.1f" % y) if y < 100 else "%.0f" % y


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--format", default="pdf")
    args = ap.parse_args()

    n = len(WORKLOADS)
    bar_w = 0.74 / len(ORDER)

    # Two stacked axes sharing x; lower band gets more height (carries the detail).
    fig, (ax_hi, ax_lo) = plt.subplots(
        2, 1, sharex=True, figsize=(18, 4.5),
        gridspec_kw={"height_ratios": [1, 2.4], "hspace": 0.07})

    for gi, wl in enumerate(WORKLOADS):
        for j, sys in enumerate(ORDER):
            rec = pick_largest(load(os.path.join(ROOT, sys, "results_%s.csv" % wl["file"])),
                               wl["scol"])
            if not rec:
                print("  ! missing %s/%s" % (sys, wl["file"]))
                continue
            x = gi + (j - (len(ORDER) - 1) / 2) * bar_w
            mk = (fnum(rec.get("makespan_mean_ms")) or 0.0) / 1000.0
            te = fnum(rec.get("total_job_mean_ms"))   # may be absent (faasm finra)
            te = te / 1000.0 if te is not None else None
            w = bar_w * 0.9
            base = SYS[sys]["color"]
            # Same bars on both axes; each axis clips to its own band.
            for ax in (ax_hi, ax_lo):
                if te is not None:
                    ax.bar(x, te, w, color=lighten(base), edgecolor="black",
                           linewidth=0.4, zorder=2)
                ax.bar(x, mk, w, color=base, edgecolor="black", linewidth=0.4,
                       zorder=3)
            # value labels: total-exec on top (on whichever band holds it),
            # makespan inside the base (always in the lower band).
            if te is not None:
                ax_te = ax_hi if te >= LOW_TOP else ax_lo
                ax_te.text(x, te, fmt(te), ha="center", va="bottom",
                           fontsize=VALUE_SIZE, color="#333", fontweight="bold",
                           zorder=5)
            ax_lo.text(x, mk, fmt(mk), ha="center", va="top", fontsize=VALUE_SIZE,
                       color="white", fontweight="bold", zorder=5)

    ax_hi.set_ylim(HIGH_BOT, HIGH_TOP)
    ax_lo.set_ylim(0, LOW_TOP)
    for ax in (ax_hi, ax_lo):
        ax.tick_params(axis="y", labelsize=TICK_SIZE)
        ax.grid(True, axis="y", alpha=.3)
        ax.set_axisbelow(True)

    # hide spines/ticks at the break and draw the diagonal break marks
    ax_hi.spines["bottom"].set_visible(False)
    ax_lo.spines["top"].set_visible(False)
    ax_hi.tick_params(axis="x", bottom=False)
    d = 0.5
    kw = dict(marker=[(-1, -d), (1, d)], markersize=12, linestyle="none",
              color="k", mec="k", mew=1, clip_on=False)
    ax_hi.plot([0, 1], [0, 0], transform=ax_hi.transAxes, **kw)
    ax_lo.plot([0, 1], [1, 1], transform=ax_lo.transAxes, **kw)

    ax_lo.set_xticks(range(n))
    # bold workload name as the tick label; size on a second line in normal weight
    ax_lo.set_xticklabels([wl["name"] for wl in WORKLOADS], fontsize=LABEL_SIZE,
                          fontweight="bold")
    for gi, wl in enumerate(WORKLOADS):
        ax_lo.annotate(wl["size"], xy=(gi, 0), xycoords=("data", "axes fraction"),
                       xytext=(0, -25), textcoords="offset points",
                       ha="center", va="top", fontsize=LABEL_SIZE)
    ax_lo.set_xlim(-0.6, n - 0.4)
    # trim the left/top whitespace; keep just enough room for the ticks + y-label
    fig.subplots_adjust(left=0.06, right=0.998, top=0.9, bottom=0.09)
    fig.supylabel("Time (s)", fontsize=YLABEL_SIZE, x=0.005)

    # legend: system colours + the shade encoding in the title, snug above top band
    handles = [Patch(facecolor=SYS[s]["color"], edgecolor="black", linewidth=0.4,
                     label=SYS[s]["label"]) for s in ORDER]
    leg = fig.legend(handles=handles, ncol=2, loc="lower center",
                     bbox_to_anchor=(0.5, 0.98), bbox_transform=ax_hi.transAxes,
                     frameon=False, fontsize=LEGEND_SIZE, columnspacing=1.8,
                     handletextpad=0.6, labelspacing=0.3, borderpad=0.2,
                     title="full bar = total execution time   ·   saturated base = makespan")
    leg.get_title().set_fontsize(LEGEND_SIZE - 1)

    out = os.path.join(FIGS, "inter_node_bars.%s" % args.format)
    fig.savefig(out, bbox_inches="tight", pad_inches=0.03)
    print("wrote", out)
    os.makedirs(FIGURES, exist_ok=True)
    dst = os.path.join(FIGURES, os.path.basename(out))
    shutil.copy(out, dst)
    print("copied to", dst)


if __name__ == "__main__":
    main()
