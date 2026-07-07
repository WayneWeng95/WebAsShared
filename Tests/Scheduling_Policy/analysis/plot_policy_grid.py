#!/usr/bin/env python3
"""plot_policy_grid.py — makespan vs cost, pack vs balanced.

One figure summarising the Scheduling_Policy sweep for all three workloads, at a
single fan-out (32), as a 2x3 grid:

  COLUMNS = workload: word_count, finra, ml_training.
  TOP ROW    = makespan (end-to-end latency, s).
  BOTTOM ROW = cost (accumulated execution time = Σ busy node-seconds).
  x-axis  = input size (3 points per workload).
  bars    = policy {pack, balanced}, coloured by policy.

The story: balanced spreads over 4 nodes -> lower makespan (top row); pack
concentrates on fewer nodes -> lower cost (bottom row, fewer node-seconds /
overhead). Reads the uniform *_exec.csv files.
Output: figs/policy_grid.pdf
"""
import csv
import os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import Patch

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, "figs")
os.makedirs(FIGS, exist_ok=True)

# colour = policy: two tones of blue (green is reserved for a compared system)
POLICY_COLOR = {"pack": "#8aa8f0", "balanced": "#3a5fc4"}  # light + WasMem blue (pop)

# Single fan-out, pack vs balanced. The node count placement (pack -> 2 nodes,
# balanced -> 4) is what drives the makespan/cost trade-off shown here.
FANOUT = 32
COMBOS = [("pack", FANOUT), ("balanced", FANOUT)]

# Per-workload config: display name, exec-csv, size column, and the (value,label)
# input-size points to show.
WORKLOADS = [
    dict(name="WordCount", csv="results_exec.csv", scol="size_mb",
         sizes=[(50, "50 MB"), (500, "500 MB"), (1001, "1 GB")]),
    dict(name="Finra", csv="results_finra_exec.csv", scol="trades",
         sizes=[(10000, "10k"), (100000, "100k"), (1000000, "1M")]),
    dict(name="ML training", csv="results_ml_training_exec.csv", scol="samples",
         sizes=[(100000, "100k"), (300000, "300k"), (600000, "600k")]),
]

# Rows of the grid: (csv column, y-axis label). Both are stored in ms -> seconds.
METRICS = [
    ("makespan_ms", "Makespan (s)"),
    ("total_exec_ms", "Cost (Accumulated\nSandbox Time)"),
]

TICK_SIZE = 13
LABEL_SIZE = 14
LEGEND_SIZE = 13
VALUE_SIZE = 11   # on-bar value labels (bold)
YLABEL_SIZE = 14  # row axis label


def load(path):
    with open(path) as f:
        return [dict(r) for r in csv.DictReader(f)]


def cell(rows, scol, size, policy, fanout):
    # tolerant size match: sweep points may be off by a rounding unit
    # (e.g. samples 99999 vs 100000) but are spaced far apart.
    for r in rows:
        if (abs(float(r[scol]) - size) <= 0.02 * size and r["policy"] == policy
                and int(r["fanout"]) == fanout):
            return r
    return None


def fmt_val(v):
    return f"{v:.0f}" if v >= 100 else f"{v:.1f}"


def main():
    fig, axes = plt.subplots(len(METRICS), len(WORKLOADS), figsize=(9, 4.5),
                             sharex="col", sharey="col")

    rows_by = {wl["name"]: load(os.path.join(HERE, wl["csv"])) for wl in WORKLOADS}

    group_w = 0.8
    bar_w = group_w / len(COMBOS)

    for col, wl in enumerate(WORKLOADS):
        rows = rows_by[wl["name"]]
        nsz = len(wl["sizes"])
        for row, (mcol, ylab) in enumerate(METRICS):
            ax = axes[row][col]
            for j, (pol, fan) in enumerate(COMBOS):
                xs, ys = [], []
                for si, (size, _) in enumerate(wl["sizes"]):
                    rec = cell(rows, wl["scol"], size, pol, fan)
                    if not rec:
                        continue
                    xs.append(si + (j - (len(COMBOS) - 1) / 2) * bar_w)
                    ys.append(float(rec[mcol]) / 1000.0)  # ms -> s
                w = bar_w * 0.92
                ax.bar(xs, ys, w, color=POLICY_COLOR[pol],
                       edgecolor="black", linewidth=0.4)
                # WordCount cost bars sit near-equal height at 1 GB, so their
                # centred labels collide; fan them apart (pack left, balanced right).
                dx = 0.0
                if wl["name"] == "WordCount" and mcol == "total_exec_ms":
                    dx = (-1 if j == 0 else 1) * bar_w * 0.18
                for x, v in zip(xs, ys):
                    ax.text(x + dx, v, fmt_val(v), ha="center", va="bottom",
                            fontsize=VALUE_SIZE, color="#333", fontweight="bold")
            ax.set_xticks(range(nsz))
            ax.set_xticklabels([lbl for _, lbl in wl["sizes"]], fontsize=TICK_SIZE)
            ax.tick_params(axis="y", labelsize=TICK_SIZE)
            ax.margins(y=0.18)
            ax.grid(True, axis="y", alpha=.3)
            if col == 0:
                ax.set_ylabel(ylab, fontsize=YLABEL_SIZE)
            # workload notation at the BOTTOM of each column (below the size ticks)
            if row == len(METRICS) - 1:
                ax.text(0.5, -0.22, wl["name"], transform=ax.transAxes,
                        ha="center", va="top", fontsize=LABEL_SIZE, fontweight="bold")

    # legend: policy colour only (pack/balanced).
    policy_h = [Patch(facecolor=POLICY_COLOR[p], edgecolor="black", linewidth=0.4,
                      label=p) for p in ("pack", "balanced")]
    fig.tight_layout(rect=[0, 0, 1, 0.94], h_pad=1.2, w_pad=1.6)
    fig.legend(handles=policy_h, loc="upper center", ncol=2, frameon=False,
               fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.02),
               columnspacing=1.8, handletextpad=0.6)
    out = os.path.join(FIGS, "policy_grid.pdf")
    fig.savefig(out, bbox_inches="tight", pad_inches=0.02)
    print("wrote", out)


if __name__ == "__main__":
    main()
