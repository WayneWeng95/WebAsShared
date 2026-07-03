#!/usr/bin/env python3
"""plot_mem_9c.py — memory saved by WasMem at each workload's largest load, from
the 9-node cluster results, vs ALL THREE baselines.

Same "memory saved by WasMem (%)" grouped bars as the intra-node
plot_mem_largest.py. Total memory per system = the run's peak working set:
  WasMem     = Σ per-node (rss + SHM arena)                 [wasmem_<wl>.csv total_mem_mb]
  Cloudburst = Σ executor-pod anon + Redis peak (KV incl.)  [cloudburst_<wl>.csv mem_total_mb + redis_peak_mb]
  RMMap      = Σ per-node MemAvailable-delta                [mem_baselines.csv]
  Faasm      = Σ per-node MemAvailable-delta (+Redis state) [mem_baselines.csv]
RMMap/Faasm were measured with the same MemAvailable-delta sampler Cloudburst uses
(mem_wrap.py + mem_probe.MemSampler); WasMem via its own rss+SHM accounting (both
count tmpfs/anon, exclude page cache — comparable).

Output: Tests/Figures/mem_largest_load_9c.pdf
"""
import csv, os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
TESTS = os.path.dirname(HERE)
FIGURES = os.path.join(TESTS, "Figures")
os.makedirs(FIGURES, exist_ok=True)

TICK_SIZE = 16; LABEL_SIZE = 17; LEGEND_SIZE = 15; YLABEL_SIZE = 17
# colors match the old mem_largest_load.pdf (plot_mem_largest.py)
COLOR = {"Cloudburst": "#e0be72", "RMMap": "#86b07d", "Faasm": "#6fa8a0"}
BASELINES = ["Cloudburst", "RMMap", "Faasm"]

WORKLOADS = [
    ("wordcount",    "Wordcount\n4 GB"),
    ("terasort",     "Terasort\n1.5 GB"),
    ("finra",        "Finra\n5M"),
    ("matrix",       "Matrix\n4096 × 4096"),
    ("ml_training",  "ML training\n10M"),
    ("ml_inference", "ML inference\n10M"),
]


def fnum(x):
    try: return float(x)
    except (TypeError, ValueError): return None


def row_of(sysdir, wl):
    path = os.path.join(HERE, sysdir, "%s_%s.csv" % (sysdir, wl))
    if not os.path.exists(path): return None
    with open(path) as f:
        rows = [dict(r) for r in csv.DictReader(f)]
    return rows[-1] if rows else None


def load_baselines_mem():
    """rmmap/faasm totals from mem_baselines.csv → {(sys,wl): total_mb}."""
    d = {}
    p = os.path.join(HERE, "mem_baselines.csv")
    if os.path.exists(p):
        with open(p) as f:
            for r in csv.DictReader(f):
                d[(r["system"], r["workload"])] = fnum(r["total_mb"])
    return d


BMEM = load_baselines_mem()


def wasmem_total(wl):
    # all four systems now measured with the SAME whole-cluster AnonPages+Shmem
    # peak sampler (mem_anon.py) → mem_baselines.csv, for a consistent comparison.
    return BMEM.get(("wasmem", wl))

def total_for(sys, wl):
    return BMEM.get((sys.lower(), wl))


labels = [lab for _, lab in WORKLOADS]
saving = {b: [] for b in BASELINES}
for wl, _ in WORKLOADS:
    ours = wasmem_total(wl)
    for b in BASELINES:
        base = total_for(b, wl)
        saving[b].append((1 - ours / base) * 100 if (ours and base) else np.nan)

x = np.arange(len(labels)); nb = len(BASELINES); wb = 0.26
fig, ax = plt.subplots(figsize=(11, 4.6))
ax.axhline(0.0, color="#444", linewidth=1, zorder=0)
for i, b in enumerate(BASELINES):
    off = (i - (nb - 1) / 2) * wb
    bars = ax.bar(x + off, saving[b], wb, label=b, color=COLOR[b],
                  edgecolor="black", linewidth=0.4)
    for bar, s in zip(bars, saving[b]):
        if np.isnan(s): continue
        ax.annotate(f"{s:.0f}%", (bar.get_x() + bar.get_width() / 2, s),
                    ha="center", va="bottom" if s >= 0 else "top", fontsize=10,
                    fontweight="bold", xytext=(0, 2 if s >= 0 else -2),
                    textcoords="offset points")

ax.set_ylabel("Memory saved by WasMem (%)", fontsize=YLABEL_SIZE)
ax.set_xticks(x)
names = [lab.split("\n")[0] for lab in labels]
sizes = [lab.split("\n")[1] for lab in labels]
ax.set_xticklabels(names, fontsize=12, fontweight="bold")
for xi, sz in zip(x, sizes):
    ax.annotate(sz, xy=(xi, 0), xycoords=("data", "axes fraction"),
                xytext=(0, -25), textcoords="offset points",
                ha="center", va="top", fontsize=13)
ax.tick_params(axis="y", labelsize=TICK_SIZE)
ax.grid(axis="y", alpha=0.3)
allv = [s for b in BASELINES for s in saving[b] if not np.isnan(s)]
ax.set_ylim(min(0, min(allv)) - 8, max(allv) + 12)
fig.tight_layout(rect=[0, 0, 1, 0.95], pad=0.4)
ax.legend(ncol=3, fontsize=LEGEND_SIZE, loc="lower center", bbox_to_anchor=(0.5, 0.99),
          frameon=False, borderaxespad=0.0)

out = os.path.join(FIGURES, "mem_largest_load_9c.pdf")
fig.savefig(out, bbox_inches="tight")
fig.savefig("/tmp/mem_largest_load_9c.png", dpi=110, bbox_inches="tight")
print("wrote", out)
print("\nmemory (MB) & saving%:")
hdr = f"{'workload':<14}{'WasMem':>9}" + "".join(f"{b:>22}" for b in BASELINES)
print(hdr)
for j, (wl, _) in enumerate(WORKLOADS):
    ours = wasmem_total(wl)
    line = f"{wl:<14}{(ours or 0):>9.0f}"
    for b in BASELINES:
        base = total_for(b, wl); s = saving[b][j]
        line += f"{(base or 0):>13.0f} ({'' if np.isnan(s) else f'{s:+.0f}%'})".rjust(22)
    print(line)


if __name__ == "__main__":
    pass
