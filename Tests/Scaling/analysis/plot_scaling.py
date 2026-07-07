#!/usr/bin/env python3
# plot_scaling.py — compact 2×3 scaling overview for WordCount + MediaReview.
#
#   top row  (strong): speedup · makespan · peak mem/node
#   bottom   (weak):   makespan · throughput scaling · peak mem (/node + cluster)
#
# x-axis is the executor count (16/48/96/144), equally spaced. One representative
# policy per workload (balanced for word_count, n/a for streaming). Reads the four
# sweep CSVs; skips missing series gracefully. → figs/scaling.pdf + .png
import csv, os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

# consistent compact font scheme for the small (9×4.5) canvas
plt.rcParams.update({
    "font.size": 7,
    "axes.titlesize": 8,
    "axes.labelsize": 7,
    "xtick.labelsize": 6.5,
    "ytick.labelsize": 6.5,
    "legend.fontsize": 5.8,
    "lines.linewidth": 1.4,
    "lines.markersize": 4,
})

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, "figs"); os.makedirs(FIGS, exist_ok=True)

WORKLOADS = {
    "word_count":  dict(color="#3b6ea5", marker="o", label="WordCount"),
    "mediareview": dict(color="#a5533b", marker="s", label="MediaReview"),
}
FILES = {
    ("word_count","strong"):  "results_wc_strong.csv",
    ("word_count","weak"):    "results_wc_weak.csv",
    ("mediareview","strong"): "results_stream_strong.csv",
    ("mediareview","weak"):   "results_stream_weak.csv",
}

def load(workload, mode):
    # one representative policy per workload (balanced for word_count, n/a for streaming);
    # the pack-vs-balanced contrast lives in plot_speedup.py.
    path = os.path.join(HERE, FILES[(workload,mode)])
    if not os.path.exists(path): return []
    with open(path) as f:
        rows = list(csv.DictReader(f))
    pols = {r.get("policy","n/a") for r in rows}
    keep = "balanced" if "balanced" in pols else next(iter(pols))
    rows = [r for r in rows if r.get("policy","n/a")==keep]
    rows = [r for r in rows if int(r["threads"]) in (16, 48, 96, 144)]  # match the 4-point grid
    rows.sort(key=lambda r: int(r["threads"]))
    return rows

def col(rows, *keys):
    for k in keys:
        if rows and k in rows[0]:
            return [float(r[k]) if r[k] not in ("","NA") else None for r in rows]
    return [None]*len(rows)

ticks = [16, 48, 96, 144]
XP = {t: i for i, t in enumerate(ticks)}        # categorical x → equal spacing
xp = lambda thr: list(thr)

fig, ax = plt.subplots(2, 3, figsize=(9, 4.5))
(ax_sp, ax_sms, ax_smem), (ax_wt, ax_wthr, ax_wmem) = ax

any_strong = any_weak = False
for wl, sty in WORKLOADS.items():
    c, m, lab = sty["color"], sty["marker"], sty["label"]
    s = load(wl, "strong")
    if s:
        any_strong = True
        thr = [int(r["threads"]) for r in s]; X = xp(thr)
        ms = col(s, "wall_ms_median")
        speedup = [ms[0]/v if (v and ms[0]) else None for v in ms]
        ax_sp.plot(X, speedup, marker=m, color=c, label=lab)
        ax_sms.plot(X, ms, marker=m, color=c, label=lab)
        ax_smem.plot(X, col(s, "peak_mem_mb_per_node"), marker=m, color=c, label=lab)
    w = load(wl, "weak")
    if w:
        any_weak = True
        thr = [int(r["threads"]) for r in w]; X = xp(thr)
        ms = col(w, "wall_ms_median")
        tp = col(w, "throughput_mb_s", "throughput_ev_s")
        tp0 = tp[0] if tp and tp[0] else 1
        ax_wt.plot(X, ms, marker=m, color=c, label=lab)
        ax_wthr.plot(X, [(t/tp0 if t else None) for t in tp], marker=m, color=c, label=lab)
        ax_wmem.plot(X, col(w, "peak_mem_mb_per_node"), marker=m, color=c, label=f"{lab} /node")
        ax_wmem.plot(X, col(w, "peak_mem_mb_total"), marker=m, color=c, ls=":", alpha=.6, label=f"{lab} cluster")

if any_strong:
    ax_sp.plot(ticks, [t/16 for t in ticks], "k--", alpha=.5, label="ideal")
if any_weak:
    ax_wthr.plot(ticks, [t/16 for t in ticks], "k--", alpha=.5, label="ideal")

ax_sp.set(title="Strong — speedup",        ylabel="× vs 16 exec.")
ax_sms.set(title="Strong — makespan",      ylabel="ms")
ax_smem.set(title="Strong — peak mem/node", ylabel="MB / node")
ax_wt.set(title="Weak — makespan",         ylabel="ms")
ax_wthr.set(title="Weak — throughput",     ylabel="× vs 16 exec.")
ax_wmem.set(title="Weak — peak memory",    ylabel="MB")

bottom = (ax_wt, ax_wthr, ax_wmem)
for a in (ax_sp, ax_sms, ax_smem, *bottom):
    a.set_xticks(ticks); a.set_xticklabels([f"{t}\n({t//16}-node)" for t in ticks])
    a.grid(True, alpha=.3)
for a in bottom:
    a.set_xlabel("number of executors")
ax_sp.legend(loc="upper left")
ax_wmem.legend(loc="upper left")

fig.suptitle("WasMem scaling — 16→144 executors (16/node)", fontweight="bold", fontsize=9)
fig.tight_layout(rect=(0, 0, 1, 0.96))
for ext in ("pdf", "png"):
    fig.savefig(os.path.join(FIGS, f"scaling.{ext}"), dpi=200)
print(f"wrote {FIGS}/scaling.pdf + .png")
