#!/usr/bin/env python3
"""plot_mem_largest.py — grouped bar chart of peak memory (incl. KV) at each
workload's LARGEST load group, across all five frameworks.

Reads mem_footprint_raw.csv (Faasm = raw single-instance RSS, NOT the concurrent
N x estimate). Largest load per workload = max input size x max fan-out.
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

FRAMEWORKS = ["WasMem-AOT", "RMMap", "Faasm", "Cloudburst"]
COLOR = {
    "WasMem-AOT":  "#2057c7",   # engine blue (ours, AOT only)
    "RMMap":       "#1d7a3e",   # green
    "Faasm":       "#2a9d8f",   # teal
    "Cloudburst":  "#edae49",   # amber
}

# (workload, label, filter dict) — largest load group per workload
LARGEST = [
    ("WordCount",    "WordCount\n1001MB/16w", {"size_mb": 1001.0, "workers": 16.0}),
    ("Finra",        "Finra\n1M trades",      {"size_trades": 1000000.0}),
    ("Matrix",       "Matrix\n2048/16w",      {"size_n": 2048.0, "workers": 16.0}),
    ("ML_training",  "ML_training\n19.5MB/16w", {"size_mb": 19.5, "workers": 16.0}),
    ("ML_inference", "ML_inference\n19.5MB/16w", {"size_mb": 19.5, "workers": 16.0}),
]

df = pd.read_csv(os.path.join(HERE, "mem_footprint_raw.csv"))

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

fig, ax = plt.subplots(figsize=(13, 6.5))
for i, fw in enumerate(FRAMEWORKS):
    off = (i - (nfw - 1) / 2) * w
    bars = ax.bar(x + off, vals[fw], w, label=fw, color=COLOR[fw],
                  edgecolor="black", linewidth=0.4)
    for b, v in zip(bars, vals[fw]):
        if np.isnan(v):
            continue
        ax.annotate(f"{v:,.0f}", (b.get_x() + b.get_width() / 2, v),
                    ha="center", va="bottom", fontsize=8, rotation=90,
                    xytext=(0, 2), textcoords="offset points")

ax.set_ylabel("Peak memory incl. KV storage (MB)", fontsize=14)
ax.set_title("Peak memory at largest load — WasMem vs RMMap / Faasm / Cloudburst",
             fontsize=14)
ax.set_xticks(x)
ax.set_xticklabels(labels, fontsize=11)
ax.legend(ncol=4, fontsize=11, loc="upper center", bbox_to_anchor=(0.5, -0.08),
          frameon=False)
ax.grid(axis="y", alpha=0.3)
ax.set_ylim(0, max(max(v for v in vals[fw] if not np.isnan(v))
                   for fw in FRAMEWORKS) * 1.12)
fig.tight_layout()

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
