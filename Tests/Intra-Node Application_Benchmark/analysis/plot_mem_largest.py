#!/usr/bin/env python3
"""plot_mem_largest.py — grouped bar chart of peak memory (incl. KV) at each
workload's LARGEST load group, across all five frameworks.

Reads mem_footprint.csv (Faasm = concurrent footprint measured by the driver).
Largest load per workload = max input size x max fan-out.
Saves figs/mem_largest_load.pdf.
"""
import os
import pandas as pd
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, "figs")
os.makedirs(FIGS, exist_ok=True)

# Font sizes — matched to the per-workload plot.py scripts in this folder.
TICK_SIZE = 16
LABEL_SIZE = 17
LEGEND_SIZE = 17
YLABEL_SIZE = 17

FRAMEWORKS = ["WasMem-AOT", "RMMap", "Faasm", "Cloudburst"]   # = CSV column names
COLOR = {
    "WasMem-AOT":  "#5a82c2",   # muted engine blue (ours, AOT only)
    "RMMap":       "#5fa06f",   # muted green
    "Faasm":       "#5aa9a0",   # muted teal
    "Cloudburst":  "#e2c16b",   # softer amber
}
# Display labels for the figure (drop the -AOT suffix; it's just our system).
DISPLAY = {"WasMem-AOT": "WasMem"}

# (workload, label, filter dict) — largest load group per workload.
# Size labels match the all-workloads grid figure (largest size per workload).
LARGEST = [
    ("WordCount",    "Wordcount\n1 GB",          {"size_mb": 1001.0, "workers": 16.0}),
    ("TeraSort",     "TeraSort\n500 MB",         {"size_mb": 500.0, "workers": 16.0}),
    ("Finra",        "Finra\n1M",                {"size_trades": 1000000.0}),
    ("Matrix",       "Matrix\n2048 × 2048",      {"size_n": 2048.0, "workers": 16.0}),
    ("ML_training",  "ML training\n600k",        {"size_mb": 19.5, "workers": 16.0}),
    ("ML_inference", "ML inference\n600k",       {"size_mb": 19.5, "workers": 16.0}),
]

df = pd.read_csv(os.path.join(HERE, "mem_footprint.csv"))

labels, vals = [], {fw: [] for fw in FRAMEWORKS}
for wl, lab, filt in LARGEST:
    sub = df[df["workload"] == wl]
    for col, v in filt.items():
        sub = sub[sub[col] == v]
    row = sub.iloc[0]
    labels.append(lab)
    for fw in FRAMEWORKS:
        vals[fw].append(float(row[fw]) if pd.notna(row[fw]) else np.nan)

x = np.arange(len(labels))
nfw = len(FRAMEWORKS)
w = 0.19

# Memory saving of WasMem vs each baseline at the largest load:
#   saving% = (1 - WasMem / baseline) * 100
# Positive = WasMem uses less; negative = WasMem uses more. WasMem is the
# reference, so only the three baselines get bars.
BASE = "WasMem-AOT"
BASELINES = ["Cloudburst", "RMMap", "Faasm"]   # bar order within each group
saving = {fw: [] for fw in BASELINES}
for j in range(len(labels)):
    ours = vals[BASE][j]
    for fw in BASELINES:
        v = vals[fw][j]
        saving[fw].append((1 - ours / v) * 100 if (v and not np.isnan(v)) else np.nan)

nb = len(BASELINES)
wb = 0.26
fig, ax = plt.subplots(figsize=(9, 4.5))
ax.axhline(0.0, color="#444", linewidth=1, zorder=0)
for i, fw in enumerate(BASELINES):
    off = (i - (nb - 1) / 2) * wb
    bars = ax.bar(x + off, saving[fw], wb, label=DISPLAY.get(fw, fw),
                  color=COLOR[fw], edgecolor="black", linewidth=0.4)
    for b, s in zip(bars, saving[fw]):
        if np.isnan(s):
            continue
        ax.annotate(f"{s:.0f}%", (b.get_x() + b.get_width() / 2, s),
                    ha="center", va="bottom" if s >= 0 else "top", fontsize=10,
                    fontweight="bold",
                    xytext=(0, 2 if s >= 0 else -2), textcoords="offset points")

ax.set_ylabel("Memory saved by WasMem (%)", fontsize=YLABEL_SIZE)
ax.set_xticks(x)
# Slightly smaller x-tick font so the two-line workload labels (e.g. ML training
# / ML inference) don't overlap now that there are six groups.
ax.set_xticklabels(labels, fontsize=14)
ax.tick_params(axis="y", labelsize=TICK_SIZE)
ax.grid(axis="y", alpha=0.3)
allv = [s for fw in BASELINES for s in saving[fw] if not np.isnan(s)]
ax.set_ylim(0, max(allv) + 8)
fig.tight_layout(rect=[0, 0, 1, 0.96], pad=0.4)
ax.legend(ncol=3, fontsize=LEGEND_SIZE, loc="lower center", bbox_to_anchor=(0.5, 0.99),
          frameon=False, borderaxespad=0.0)

fig.savefig(os.path.join(FIGS, "mem_largest_load.pdf"), bbox_inches="tight")
print("wrote figs/mem_largest_load.pdf")
for lab, *_ in LARGEST:
    pass
print("\nvalues (MB):")
print(f"{'workload':<14}" + "".join(f"{fw:>13}" for fw in FRAMEWORKS))
for j, lab in enumerate([l[0] for l in LARGEST]):
    print(f"{lab:<14}" + "".join(
        f"{vals[fw][j]:>13,.0f}" if not np.isnan(vals[fw][j]) else f"{'CRASH':>13}"
        for fw in FRAMEWORKS))
