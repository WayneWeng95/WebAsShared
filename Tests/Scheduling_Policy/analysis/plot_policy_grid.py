#!/usr/bin/env python3
"""plot_policy_grid.py — makespan inside total execution time, pack vs balanced.

One figure summarising the Scheduling_Policy sweep for all three workloads, at a
single fan-out (32):

  COLUMNS = workload: word_count, finra, ml_training.
  x-axis  = input size (3 points per workload).
  bars    = policy {pack, balanced}, coloured by policy. Each bar's FULL height
            is the total execution time (Σ busy node-seconds); the darker SHADOW
            inside it (from the baseline) is the makespan (end-to-end latency).
            The working-node count is printed on top (pack -> 2, balanced -> 4).

The story: balanced spreads over 4 nodes -> lower makespan (shorter shadow); pack
concentrates on fewer nodes -> lower total_exec (shorter full bar, less
node-seconds / overhead). Reads the uniform *_exec.csv files.
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

# Single fan-out, pack vs balanced. The node count printed on each bar shows the
# placement (pack -> 2 nodes, balanced -> 4).
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
OUTER_ALPHA = 0.38  # translucent total-exec bar; makespan shadow is opaque inside

TICK_SIZE = 13
LABEL_SIZE = 14
LEGEND_SIZE = 13
VALUE_SIZE = 12   # on-bar time labels (bold)
YLABEL_SIZE = 17  # "Time (s)" axis label


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


def lighten(hexc, f=0.62):
    """Lighter tint of a hex color (toward white) for the total-execution-time
    envelope, so the full bar reads as a pale halo around the saturated makespan
    base (= the policy color)."""
    r, g, b = (int(hexc[i:i + 2], 16) for i in (1, 3, 5))
    return "#%02x%02x%02x" % tuple(int(c + (255 - c) * f) for c in (r, g, b))


def main():
    nrows = len(WORKLOADS)
    fig, axes = plt.subplots(nrows, 1, figsize=(9, 6))

    rows_by = {wl["name"]: load(os.path.join(HERE, wl["csv"])) for wl in WORKLOADS}

    group_w = 0.8
    bar_w = group_w / len(COMBOS)

    for c, wl in enumerate(WORKLOADS):
        rows = rows_by[wl["name"]]
        nsz = len(wl["sizes"])
        ax = axes[c]
        for j, (pol, fan) in enumerate(COMBOS):
            xs, exec_ys, mk_ys = [], [], []
            for si, (size, _) in enumerate(wl["sizes"]):
                rec = cell(rows, wl["scol"], size, pol, fan)
                if not rec:
                    continue
                xs.append(si + (j - (len(COMBOS) - 1) / 2) * bar_w)
                exec_ys.append(float(rec["total_exec_ms"]) / 1000.0)  # ms -> s
                mk_ys.append(float(rec["makespan_ms"]) / 1000.0)
            w = bar_w * 0.92
            # Opaque two-tone bar: full height = total execution time (LIGHTER tint
            # of the policy color), saturated base = makespan (the policy color).
            mk_color = POLICY_COLOR[pol]
            mk_label = "white"   # white makespan labels on both policies
            ax.bar(xs, exec_ys, w, color=lighten(mk_color),
                   edgecolor="black", linewidth=0.4)
            ax.bar(xs, mk_ys, w, color=mk_color,
                   edgecolor="black", linewidth=0.4)
            # value labels: total exec on top of the bar, makespan inside the base
            for x, te, mk in zip(xs, exec_ys, mk_ys):
                ax.text(x, te, f"{te:.1f}", ha="center", va="bottom",
                        fontsize=VALUE_SIZE, color="#333", fontweight="bold")
                ax.text(x, mk, f"{mk:.1f}", ha="center", va="top",
                        fontsize=VALUE_SIZE, color=mk_label, fontweight="bold")
        ax.set_xticks(range(nsz))
        ax.set_xticklabels([lbl for _, lbl in wl["sizes"]], fontsize=TICK_SIZE)
        ax.tick_params(axis="y", labelsize=TICK_SIZE)
        ax.margins(y=0.16)
        ax.grid(True, axis="y", alpha=.3)
        ax.set_ylabel("Time (s)", fontsize=YLABEL_SIZE)
        # workload notation at the BOTTOM of each panel (below the size ticks)
        ax.text(0.5, -0.22, wl["name"], transform=ax.transAxes,
                ha="center", va="top", fontsize=LABEL_SIZE, fontweight="bold")

    # legend: policy colour only (pack/balanced); the shade encoding goes in the
    # legend title so it isn't confused with mismatched grey swatches.
    policy_h = [Patch(facecolor=POLICY_COLOR[p], edgecolor="black", linewidth=0.4,
                      label=p) for p in ("pack", "balanced")]
    # reserve a slim band ABOVE the plots for the legend (title = encoding
    # explanation, row below = pack/balanced) so nothing sits inside the top panel.
    fig.tight_layout(rect=[0, 0, 1, 0.93], h_pad=0.4)
    leg = fig.legend(handles=policy_h, loc="upper center", ncol=2, frameon=False,
                     fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.02),
                     columnspacing=1.8, handletextpad=0.6,
                     title="full bar = total execution time   ·   saturated base = makespan")
    leg.get_title().set_fontsize(LEGEND_SIZE - 1)
    out = os.path.join(FIGS, "policy_grid.pdf")
    fig.savefig(out, bbox_inches="tight", pad_inches=0.02)
    print("wrote", out)


if __name__ == "__main__":
    main()
