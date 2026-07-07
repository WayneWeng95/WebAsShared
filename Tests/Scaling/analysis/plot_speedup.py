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
TICK_SIZE = 12
LABEL_SIZE = 15
LEGEND_SIZE = 14
VALUE_SIZE = 11
YLABEL_SIZE = 16

# One drawn series per (workload, placement policy). word_count carries two
# policies (balanced, pack) — drawn distinctly so any divergence is visible (they
# should overlap: WordCount is the policy-insensitive control). streaming is a
# single "n/a" data-parallel series.
COLOR = {"word_count": "#3b6ea5", "mediareview": "#a5533b"}
WLABEL = {"word_count": "WordCount (batch)", "mediareview": "MediaReview (streaming)"}
# weak-panel throughput annotation unit per workload (WordCount = MB/s, MediaReview = ev/s)
THRU_FMT = {"word_count": lambda t: f"{t:.0f}\nMB/s", "mediareview": lambda t: f"{t/1000:.0f}k\nev/s"}
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
fig, (ax_s, ax_w) = plt.subplots(1, 2, figsize=(9, 4.5), sharex=True)

ymax = 9.5
for wl in COLOR:
    for pol, s in series(wl, "strong"):
        if pol == "pack": continue                          # drop the pack sanity control
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
        # weak scaling grows the load with the thread count, so raw throughput
        # scaling (tp/tp0) rises to p/16 ideally. Normalise by the load ratio
        # (load/load0) so the ideal line is a flat 1× — i.e. per-unit-of-work speedup.
        ld = colf(w, "total_mb", "events"); ld0 = ld[0]
        sc = [(v/tp0)/(l/ld0) if (v and tp0 and l and ld0) else None for v,l in zip(tp,ld)]
        X = list(thr)
        ax_w.plot(X, sc, marker=ps["marker"], ls=ps["ls"], color=COLOR[wl], lw=2, ms=8,
                  mfc=(COLOR[wl] if ps["mfc"]=="full" else "white"), label=WLABEL[wl])
        # label each point with its ABSOLUTE throughput (WordCount MB/s, MediaReview k ev/s)
        fmt = THRU_FMT[wl]
        # WordCount curve sits above MediaReview → label it above, MediaReview below (max separation)
        dy, va = ((8, "bottom") if wl == "word_count" else (-8, "top"))
        for p, v, tpv in zip(thr, sc, tp):
            if v is None or tpv is None: continue
            ax_w.annotate(fmt(tpv), (p, v), textcoords="offset points",
                          xytext=(0, dy), ha="center", va=va,
                          fontsize=VALUE_SIZE, fontweight="bold", color=COLOR[wl])

for ax, title, ideal, ideal_lbl, ytop in (
    # strong: ideal speedup is linear (p/16); weak: normalised by load, so ideal is a flat 1×.
    (ax_s, "Strong scaling", [t/16 for t in ticks], "ideal (linear), = compute", ymax),
    (ax_w, "Weak scaling",   [1 for _ in ticks],    "ideal (flat), = 1×",         1.8),
):
    ax.plot(ticks, ideal, "k--", alpha=.55, label=ideal_lbl)
    ax.set_xticks(ticks)
    ax.set_xticklabels([f"{t}\n({t//16}-node)" for t in ticks])   # thread count + node count
    ax.tick_params(axis="both", labelsize=TICK_SIZE)
    ax.set_ylabel("speedup", fontsize=YLABEL_SIZE)
    # x-tick labels already carry executor + node counts, so no separate "number of
    # executors" axis label — just the bold panel title below the two-line ticks.
    ax.text(0.5, -0.22, title, transform=ax.transAxes, ha="center", fontweight="bold", fontsize=18)
    ax.set_ylim(0, ytop); ax.grid(True, alpha=.3); ax.legend(loc="upper left", fontsize=LEGEND_SIZE)
    ax.set_xlim(ticks[0] - 12, ticks[-1] + 26)   # room for the wide edge labels (32 MB/s … 529k ev/s)

fig.subplots_adjust(left=0.075, right=0.99, top=0.97, bottom=0.22, wspace=0.20)
for ext in ("pdf","png"):
    fig.savefig(os.path.join(FIGS, f"scaling_speedup.{ext}"), dpi=150,
                bbox_inches="tight", pad_inches=0.03)
print(f"wrote {FIGS}/scaling_speedup.pdf + .png")
