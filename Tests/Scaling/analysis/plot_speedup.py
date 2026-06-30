#!/usr/bin/env python3
# plot_speedup.py — speedup vs. compute power (the headline scaling graph).
#
# x-axis = compute power = threads (16 / node), i.e. 16,48,96,144 = 1×,3×,6×,9×.
# y-axis = how much faster / more throughput you get for that much more compute,
#          measured against the 16-thread point, with the IDEAL LINEAR line (y = p/16)
#          for reference. (No efficiency ratio — just raw scaling vs. compute.)
#
#   left : strong scaling — speedup            = T(16) / T(p)           (fixed total work)
#   right: weak scaling   — throughput scaling = thr(p) / thr(16)       (fixed work/thread)
#
# Reads the four sweep CSVs. → figs/scaling_speedup.{pdf,png}
import csv, os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, "figs"); os.makedirs(FIGS, exist_ok=True)

# font sizes (pt)
TICK_SIZE = 16
LABEL_SIZE = 15
LEGEND_SIZE = 14
VALUE_SIZE = 12
YLABEL_SIZE = 16

# One drawn series per (workload, placement policy). word_count carries two
# policies (balanced, pack) — drawn distinctly so any divergence is visible (they
# should overlap: WordCount is the policy-insensitive control). streaming is a
# single "n/a" data-parallel series.
COLOR = {"word_count": "#3b6ea5", "mediareview": "#a5533b"}
WLABEL = {"word_count": "WordCount (batch)", "mediareview": "MediaReview (streaming)"}
POL_STYLE = {  # marker / linestyle / fill per policy so overlapping lines stay legible
    "balanced": dict(marker="o", ls="-",  mfc="full"),
    "pack":     dict(marker="s", ls="--", mfc="none"),
    "n/a":      dict(marker="D", ls="-",  mfc="full"),
}
FILES = {
    ("word_count","strong"):  "results_wc_strong.csv",
    ("word_count","weak"):    "results_wc_weak.csv",
    ("mediareview","strong"): "results_stream_strong.csv",
    ("mediareview","weak"):   "results_stream_weak.csv",
}

def series(workload, mode):
    """yield (policy, rows-sorted-by-threads) for each policy in the file."""
    path = os.path.join(HERE, FILES[(workload,mode)])
    if not os.path.exists(path): return
    with open(path) as f:
        rows = list(csv.DictReader(f))
    for pol in sorted({r.get("policy","n/a") for r in rows}):
        sr = sorted((r for r in rows if r.get("policy","n/a")==pol), key=lambda r: int(r["threads"]))
        if sr: yield pol, sr

def colf(rows, *keys):
    for k in keys:
        if rows and k in rows[0]:
            return [float(r[k]) if r[k] not in ("","NA") else None for r in rows]
    return [None]*len(rows)

def lbl(wl, pol):
    return WLABEL[wl] if pol=="n/a" else f"{WLABEL[wl]} · {pol}"

ticks = [16, 48, 96, 144]
# x positions = the executor count itself (proportional spacing), so the ideal line
# y = executors/16 plots as a straight line through the origin.
XP = {t: t for t in ticks}
fig, (ax_s, ax_w) = plt.subplots(1, 2, figsize=(11, 4.5), sharex=True)

ymax = 9.5
for wl in COLOR:
    for pol, s in series(wl, "strong"):
        if pol == "pack" or wl == "mediareview": continue  # drop pack; omit MediaReview from strong panel
        ps = POL_STYLE.get(pol, POL_STYLE["n/a"])
        thr = [int(r["threads"]) for r in s]; t = colf(s, "wall_ms_median"); t0 = t[0]
        sp = [t0/ti if (ti and t0) else None for ti in t]
        X = list(thr)
        ax_s.plot(X, sp, marker=ps["marker"], ls=ps["ls"], color=COLOR[wl], lw=2, ms=8,
                  mfc=(COLOR[wl] if ps["mfc"]=="full" else "white"), label=WLABEL[wl])
        for p,v in zip(thr,sp):
            if v and f"{v:.1f}" != "1.0":     # below the node; drop 1.0× baseline
                ax_s.annotate(f"{v:.1f}×", (p,v), textcoords="offset points",
                              xytext=(0,-8), ha="center", va="top",
                              fontsize=VALUE_SIZE, fontweight="bold", color=COLOR[wl])
    for pol, w in series(wl, "weak"):
        if pol == "pack": continue          # weak: balanced only for WordCount
        w = [r for r in w if int(r["threads"]) in ticks]   # only 16/48/96/144 points
        ps = POL_STYLE.get(pol, POL_STYLE["n/a"])
        thr = [int(r["threads"]) for r in w]; tp = colf(w, "throughput_mb_s", "throughput_ev_s"); tp0 = tp[0]
        sc = [v/tp0 if (v and tp0) else None for v in tp]
        X = list(thr)
        ax_w.plot(X, sc, marker=ps["marker"], ls=ps["ls"], color=COLOR[wl], lw=2, ms=8,
                  mfc=(COLOR[wl] if ps["mfc"]=="full" else "white"), label=WLABEL[wl])
        for p,v in zip(thr,sc):
            if v and f"{v:.1f}" != "1.0":     # below the node; drop 1.0× baseline
                above = (wl == "word_count" and p == 48)   # lift the first WordCount (2.4×) above
                ax_w.annotate(f"{v:.1f}×", (p,v), textcoords="offset points",
                              xytext=(0, 8 if above else -8), ha="center",
                              va="bottom" if above else "top",
                              fontsize=VALUE_SIZE, fontweight="bold", color=COLOR[wl])

for ax, title in (
    (ax_s, "Strong scaling"),
    (ax_w, "Weak scaling"),
):
    ax.plot(ticks, [t/16 for t in ticks], "k--", alpha=.55, label="ideal (linear), = compute")
    ax.set_xticks(ticks)
    ax.set_xticklabels([str(t) for t in ticks])      # proportional spacing → straight ideal
    ax.tick_params(axis="both", labelsize=TICK_SIZE)
    ax.set_ylabel("throughput scaling", fontsize=YLABEL_SIZE)
    ax.set_xlabel("number of executors", fontsize=LABEL_SIZE)
    # subplot title at the very bottom, below the axis label (pushed down to fill
    # the empty band so there's little blank under it on the fixed 9×4.5 canvas)
    ax.text(0.5, -0.24, title, transform=ax.transAxes, ha="center", fontweight="bold", fontsize=18)
    ax.set_ylim(0, ymax); ax.grid(True, alpha=.3); ax.legend(loc="upper left", fontsize=LEGEND_SIZE)

fig.subplots_adjust(left=0.085, right=0.985, top=0.97, bottom=0.20, wspace=0.22)
for ext in ("pdf","png"):
    fig.savefig(os.path.join(FIGS, f"scaling_speedup.{ext}"), dpi=150)
print(f"wrote {FIGS}/scaling_speedup.pdf + .png")
