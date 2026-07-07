#!/usr/bin/env python3
"""plot_inter_bars_9c.py — inter-node app benchmark bars from the 9-node cluster
results (Tests/9_cluster_results/<sys>/<sys>_<wl>.csv), all four systems.

2x6 grid: COLUMNS = workload (6), rows = metric.
  TOP ROW    = makespan (end-to-end latency, s); cold start (Cloudburst/RMMap)
               drawn as a hatched increment on the makespan base.
  BOTTOM ROW = cost (Accumulated Sandbox Time = Σ busy node-seconds).
Bars in each cell = the four systems, coloured by system. Each panel auto-scales
independently (cost is ~10-100x makespan, so a shared scale is unreadable).

Output: Tests/Figures/inter_node_bars_9c.pdf
"""
import csv, os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
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
    dict(name="Wordcount",    file="wordcount",    size="4 GB"),
    dict(name="Terasort",     file="terasort",     size="1.5 GB"),
    dict(name="Finra",        file="finra",        size="6M"),
    dict(name="Matrix",       file="matrix",       size="4096 × 4096"),
    dict(name="ML training",  file="ml_training",  size="10M"),
    dict(name="ML inference", file="ml_inference", size="10M"),
]

# rows of the grid: (csv column, y-axis label). Both are stored in ms -> seconds.
METRICS = [
    ("makespan_mean_ms", "Makespan (s)"),
    ("total_job_mean_ms", "Cost (Accumulated\nSandbox Time)"),
]

TICK_SIZE = 14; LABEL_SIZE = 16; LEGEND_SIZE = 17; VALUE_SIZE = 12; YLABEL_SIZE = 18


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


def fmt(y):
    return ("%.1f" % y) if y < 100 else "%.0f" % y


def main():
    nrows, ncols = len(METRICS), len(WORKLOADS)
    fig, axes = plt.subplots(nrows, ncols, figsize=(3.6 * ncols, 5.5))
    letters = "abcdefghij"

    for c, wl in enumerate(WORKLOADS):
        recs = {s: row_of(s, wl["file"]) for s in ORDER}
        for row, (mcol, ylab) in enumerate(METRICS):
            ax = axes[row][c]
            ys = []
            for s in ORDER:
                v = fnum(recs[s].get(mcol)) if recs[s] else None
                ys.append(v / 1000.0 if v is not None else 0.0)  # ms -> s
            bars = ax.bar(range(len(ORDER)), ys,
                          color=[SYS[s]["color"] for s in ORDER],
                          edgecolor="black", linewidth=0.3)
            for b, y in zip(bars, ys):
                if y:
                    ax.text(b.get_x() + b.get_width() / 2, y, fmt(y),
                            ha="center", va="bottom", fontsize=VALUE_SIZE,
                            fontweight="bold")
            # cold-start hatched increment on the makespan base (top row only)
            if mcol == "makespan_mean_ms":
                for j, s in enumerate(ORDER):
                    if not recs[s]:
                        continue
                    cold = cold_increment(s, recs[s], ys[j])
                    if cold:
                        ax.bar(j, cold, bottom=ys[j], color=SYS[s]["color"],
                               edgecolor="white", linewidth=0.5, hatch="////")
                        # thin cold increments (e.g. Finra RMMap +1.2) crowd the
                        # makespan label below, so lift them a bit more.
                        dy = 12 if (wl["name"] == "Finra" and s == "rmmap") else 5
                        ax.annotate("+%.1f" % cold, xy=(j, ys[j] + cold),
                                    xytext=(0, dy), textcoords="offset points",
                                    ha="center", va="bottom", fontsize=VALUE_SIZE,
                                    color="black", fontweight="bold")
            ax.set_xticks([])
            ax.margins(y=0.16)
            ax.tick_params(axis="y", labelsize=TICK_SIZE)
            ax.grid(True, axis="y", alpha=.3)
            ax.set_axisbelow(True)
            if c == 0:
                ax.set_ylabel(ylab, fontsize=YLABEL_SIZE)
            # size + workload notation at the BOTTOM of each column
            if row == nrows - 1:
                ax.set_xlabel(wl["size"], fontsize=LABEL_SIZE)
                ax.text(0.5, -0.15, "%s. %s" % (letters[c], wl["name"]),
                        transform=ax.transAxes, ha="center", va="top",
                        fontsize=LABEL_SIZE, fontweight="bold")

    fig.tight_layout(rect=[0.008, 0, 1, 0.94], w_pad=0.4, h_pad=1.2)

    handles = [Patch(facecolor=SYS[s]["color"], edgecolor="black", linewidth=0.3,
                     label=SYS[s]["label"]) for s in ORDER]
    handles.append(Patch(facecolor="#cfcfcf", edgecolor="white", linewidth=0.5,
                         hatch="////", label="cold start (+s)"))
    fig.legend(handles=handles, ncol=5, loc="upper center", frameon=False,
               fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.0),
               columnspacing=2.0, handletextpad=0.6)

    out = os.path.join(FIGURES, "inter_node_bars_9c.pdf")
    fig.savefig(out, bbox_inches="tight", pad_inches=0.03)
    fig.savefig("/tmp/inter_node_bars_9c.png", dpi=110, bbox_inches="tight", pad_inches=0.03)
    print("wrote", out)


if __name__ == "__main__":
    main()
