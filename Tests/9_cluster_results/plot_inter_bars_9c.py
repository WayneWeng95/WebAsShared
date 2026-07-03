#!/usr/bin/env python3
"""plot_inter_bars_9c.py — inter-node app benchmark bars from the 9-node cluster
results (Tests/9_cluster_results/<sys>/<sys>_<wl>.csv), all four systems.

Adapted from ../Inter-Node Application_Benchmark/analysis/plot_inter_bars.py:
same two-tone bars (full = total execution Σ node-seconds, saturated base =
makespan), same broken linear y-axis, same palette. Cold-start (Cloudburst/RMMap)
is derived from the CSVs' cold_makespan columns and drawn as a hatched segment on
the makespan base.

Output: Tests/Figures/inter_node_bars.pdf
"""
import csv, os, shutil
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
from matplotlib.patches import Patch

HERE = os.path.dirname(os.path.abspath(__file__))          # Tests/9_cluster_results
TESTS = os.path.dirname(HERE)                              # Tests/
FIGURES = os.path.join(TESTS, "Figures")
os.makedirs(FIGURES, exist_ok=True)

SYS = {  # canonical palette (viz_color_schemes.py / all_workloads_bars.pdf)
    "faasm":      dict(color="#6fa8a0", label="Faasm"),       # teal
    "wasmem":     dict(color="#3a5fc4", label="WasMem"),      # blue (ours)
    "cloudburst": dict(color="#e0be72", label="Cloudburst"),  # gold
    "rmmap":      dict(color="#86b07d", label="RMMap"),       # sage (was mauve = RTSFaaS's hue)
}
ORDER = ["cloudburst", "rmmap", "faasm", "wasmem"]

# per workload: display name, csv basename, size label (current run sizes)
WORKLOADS = [
    dict(name="WordCount",    file="wordcount",    size="4 GB"),
    dict(name="TeraSort",     file="terasort",     size="1.5 GB"),
    dict(name="Finra",        file="finra",        size="6M"),
    dict(name="Matrix",       file="matrix",       size="4096 × 4096"),
    dict(name="ML training",  file="ml_training",  size="10M"),
    dict(name="ML inference", file="ml_inference", size="10M"),
]

LOW_TOP = 65.0
HIGH_BOT, HIGH_TOP = 110.0, 1450.0
TICK_SIZE = 16; LABEL_SIZE = 16; LEGEND_SIZE = 17; VALUE_SIZE = 12; YLABEL_SIZE = 22


def fnum(x):
    try: return float(x)
    except (TypeError, ValueError): return None


def row_of(sysdir, wl):
    """The single data row from 9_cluster_results/<sysdir>/<sysdir>_<wl>.csv."""
    path = os.path.join(HERE, sysdir, "%s_%s.csv" % (sysdir, wl))
    if not os.path.exists(path): return None
    with open(path) as f:
        rows = [dict(r) for r in csv.DictReader(f)]
    return rows[-1] if rows else None


def cold_increment(sysdir, rec, mk):
    """cold makespan minus warm makespan (seconds); None if not measured."""
    cm = rec.get("cold_makespan_mean_ms") or rec.get("cold_makespan_ms")
    cm = fnum(cm)
    if cm is None: return None
    return max(0.0, cm / 1000.0 - mk)


def lighten(hexc, f=0.62):
    r, g, b = (int(hexc[i:i+2], 16) for i in (1, 3, 5))
    return "#%02x%02x%02x" % tuple(int(c + (255 - c) * f) for c in (r, g, b))


def fmt(y):
    return ("%.1f" % y) if y < 100 else "%.0f" % y


def main():
    n = len(WORKLOADS)
    bar_w = 0.74 / len(ORDER)
    fig, (ax_hi, ax_lo) = plt.subplots(
        2, 1, sharex=True, figsize=(18, 4.5),
        gridspec_kw={"height_ratios": [1, 2.4], "hspace": 0.07})

    for gi, wl in enumerate(WORKLOADS):
        for j, sysd in enumerate(ORDER):
            rec = row_of(sysd, wl["file"])
            if not rec:
                print("  ! missing %s/%s" % (sysd, wl["file"])); continue
            x = gi + (j - (len(ORDER) - 1) / 2) * bar_w
            mk = (fnum(rec.get("makespan_mean_ms")) or 0.0) / 1000.0
            te = fnum(rec.get("total_job_mean_ms"))
            te = te / 1000.0 if te is not None else None
            w = bar_w * 0.9
            base = SYS[sysd]["color"]
            for ax in (ax_hi, ax_lo):
                if te is not None:
                    ax.bar(x, te, w, color=lighten(base), edgecolor="black",
                           linewidth=0.4, zorder=2)
                ax.bar(x, mk, w, color=base, edgecolor="black", linewidth=0.4, zorder=3)
            cold = cold_increment(sysd, rec, mk)
            if cold:
                for ax in (ax_hi, ax_lo):
                    ax.bar(x, cold, w, bottom=mk, color=base, edgecolor="white",
                           linewidth=0.5, hatch="////", zorder=4)
                ax_lo.text(x, mk + cold, "+%.1f" % cold, ha="center", va="bottom",
                           fontsize=VALUE_SIZE - 2, color="#444", fontstyle="italic",
                           fontweight="bold", zorder=6)
            if te is not None:
                ax_te = ax_hi if te >= LOW_TOP else ax_lo
                ax_te.text(x, te, fmt(te), ha="center", va="bottom",
                           fontsize=VALUE_SIZE, color="#333", fontweight="bold", zorder=5)
            ax_lo.text(x, mk, fmt(mk), ha="center", va="top", fontsize=VALUE_SIZE,
                       color="white", fontweight="bold", zorder=5)

    ax_hi.set_ylim(HIGH_BOT, HIGH_TOP)
    ax_lo.set_ylim(0, LOW_TOP)
    for ax in (ax_hi, ax_lo):
        ax.tick_params(axis="y", labelsize=TICK_SIZE)
        ax.grid(True, axis="y", alpha=.3); ax.set_axisbelow(True)
    ax_hi.spines["bottom"].set_visible(False)
    ax_lo.spines["top"].set_visible(False)
    ax_hi.tick_params(axis="x", bottom=False)
    d = 0.5
    kw = dict(marker=[(-1, -d), (1, d)], markersize=12, linestyle="none",
              color="k", mec="k", mew=1, clip_on=False)
    ax_hi.plot([0, 1], [0, 0], transform=ax_hi.transAxes, **kw)
    ax_lo.plot([0, 1], [1, 1], transform=ax_lo.transAxes, **kw)

    ax_lo.set_xticks(range(n))
    ax_lo.set_xticklabels([wl["name"] for wl in WORKLOADS], fontsize=LABEL_SIZE,
                          fontweight="bold")
    for gi, wl in enumerate(WORKLOADS):
        ax_lo.annotate(wl["size"], xy=(gi, 0), xycoords=("data", "axes fraction"),
                       xytext=(0, -25), textcoords="offset points",
                       ha="center", va="top", fontsize=LABEL_SIZE)
    ax_lo.set_xlim(-0.6, n - 0.4)
    fig.subplots_adjust(left=0.06, right=0.998, top=0.9, bottom=0.09)
    fig.supylabel("Time (s)", fontsize=YLABEL_SIZE, x=0.005)

    handles = [Patch(facecolor=SYS[s]["color"], edgecolor="black", linewidth=0.4,
                     label=SYS[s]["label"]) for s in ORDER]
    handles.append(Patch(facecolor="#cfcfcf", edgecolor="white", linewidth=0.5,
                         hatch="////", label="cold start (+s)"))
    leg = fig.legend(handles=handles, ncol=5, loc="lower center",
                     bbox_to_anchor=(0.5, 0.98), bbox_transform=ax_hi.transAxes,
                     frameon=False, fontsize=LEGEND_SIZE, columnspacing=1.8,
                     handletextpad=0.6, labelspacing=0.3, borderpad=0.2,
                     title="full bar = total execution time   ·   saturated base = makespan")
    leg.get_title().set_fontsize(LEGEND_SIZE - 1)

    out = os.path.join(FIGURES, "inter_node_bars_9c.pdf")
    fig.savefig(out, bbox_inches="tight", pad_inches=0.03)
    fig.savefig("/tmp/inter_node_bars_9c.png", dpi=110, bbox_inches="tight", pad_inches=0.03)
    print("wrote", out)


if __name__ == "__main__":
    main()
