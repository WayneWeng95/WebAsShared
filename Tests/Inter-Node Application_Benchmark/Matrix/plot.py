#!/usr/bin/env python3
# plot.py — Matrix-multiply cross-system overlay: WebAsShared (ours) vs Cloudburst
# (SUMMA over Redis) vs RMMap-ES (parallel, over Redis) vs Faasm-like (WASM blocks
# over Redis), across matrix size {512, 1024, 2048} and block-grid workers W. All
# systems use the SAME naive (non-BLAS) block kernel, so the comparison is about
# the data substrate (zero-copy SHM vs serialized Redis), not kernel speed.
#
# Emits (styling matched to ../WordCount/plot.py and Tests/StateSync):
#   matrix_overlay.pdf : (1) GFLOP/s vs W at 2048   (2) peak GFLOP/s per size bars
#   matrix_bars.pdf    : the paper figure — one panel per size, four systems' best
#                        end-to-end latency (s), bars in legend order.
#
# Robust to CRASH rows (ours/faasm low-W OOM) and partial sweeps. ours = the AOT
# line (results_aot.csv); point OURS at results.csv for the JIT reference.
import csv
import os

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib.patches import Patch

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, 'figs')
os.makedirs(FIGS, exist_ok=True)

TICK_SIZE = 16
LABEL_SIZE = 16
LEGEND_SIZE = 17
YLABEL_SIZE = 16

OURS = os.path.join(HERE, 'results_aot.csv')        # AOT guest (fair vs AOT WASM)
CB = os.path.join(HERE, 'baseline', 'cloudburst', 'results.csv')
RM = os.path.join(HERE, 'baseline', 'rmmap', 'results.csv')
FA = os.path.join(HERE, 'baseline', 'faasm', 'demo', 'results.csv')

# Colors + markers matched to Tests/StateSync/plot_compare.py.
STYLE = {
    'ours':       dict(color='#2057c7', marker='o', label='WasMem'),      # engine blue
    'faasm':      dict(color='#2a9d8f', marker='D', label='Faasm'),       # shm-copy teal
    'rmmap':      dict(color='#1d7a3e', marker='*', label='RMMap'),       # shm-zerocopy green
    'cloudburst': dict(color='#edae49', marker='v', label='Cloudburst'),  # redis amber
}

# Per-system end-to-end latency column (differs across the CSVs).
LAT_KEY = {'ours': 'compute_ms', 'faasm': 'e2e_ms',
           'rmmap': 'e2e_ms', 'cloudburst': 'e2e_ms_median'}

SIZES = [512, 1024, 2048]
MID = 1024   # the size used for the "throughput vs W" line panel


def fnum(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None


def load(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [dict(r) for r in csv.DictReader(f)]


def gflops_vs_w(rows, size):
    """(W, GFLOP/s) for a matrix size, sorted by W."""
    pts = []
    for r in rows:
        if abs(fnum(r['size_n']) - size) > 1.5:
            continue
        t = fnum(r.get('gflops'))
        if t is None:
            continue
        pts.append((int(float(r['workers'])), t))
    return sorted(pts)


def best_gflops_by_size(rows, sizes):
    """(size, max GFLOP/s over W) — skips CRASH/missing cells."""
    out = []
    for s in sizes:
        ts = [fnum(r.get('gflops')) for r in rows
              if abs(fnum(r['size_n']) - s) <= 1.5 and fnum(r.get('gflops'))]
        if ts:
            out.append((s, max(ts)))
    return out


def lat_best_by_size(rows, sizes, key):
    """(size, MIN e2e latency in seconds over W) — best config per size."""
    out = []
    for s in sizes:
        vs = [fnum(r.get(LAT_KEY[key])) for r in rows
              if abs(fnum(r['size_n']) - s) <= 1.5 and fnum(r.get(LAT_KEY[key]))]
        if vs:
            out.append((s, min(vs) / 1000.0))
    return out


def overlay_fig(systems):
    fig, ax = plt.subplots(1, 2, figsize=(9, 4.5))
    width = 0.2
    xpos = list(range(len(SIZES)))

    # Panel 1: GFLOP/s vs W at the middle size (higher is better).
    for rows, key in systems:
        pts = [(w, t) for w, t in gflops_vs_w(rows, MID) if w >= 1]
        if pts:
            xs, ys = zip(*pts)
            ax[0].plot(xs, ys, **STYLE[key])
    ax[0].set_title('Throughput vs block-grid W  (%d×%d)' % (MID, MID), fontsize=LABEL_SIZE)
    ax[0].set_xlabel('block workers W', fontsize=LABEL_SIZE)
    ax[0].set_ylabel('Throughput (GFLOP/s)', fontsize=YLABEL_SIZE)
    ax[0].set_xscale('log', base=2)
    ax[0].set_xticks([1, 2, 4, 8, 16])
    ax[0].get_xaxis().set_major_formatter(matplotlib.ticker.ScalarFormatter())
    ax[0].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[0].grid(True, alpha=.3)

    # Panel 2: peak GFLOP/s per matrix size — grouped bars.
    for i, (rows, key) in enumerate(systems):
        bysize = dict(best_gflops_by_size(rows, SIZES))
        ys = [bysize.get(s, 0) for s in SIZES]
        ax[1].bar([x + (i - 1.5) * width for x in xpos], ys, width,
                  color=STYLE[key]['color'], label=STYLE[key]['label'])
        for k, y in enumerate(ys):
            if y:
                ax[1].text(xpos[k] + (i - 1.5) * width, y, '%.0f' % y,
                           ha='center', va='bottom', fontsize=9, fontweight='bold')
    ax[1].set_title('Peak throughput per size', fontsize=LABEL_SIZE)
    ax[1].set_xlabel('matrix size N', fontsize=LABEL_SIZE)
    ax[1].set_ylabel('Throughput (GFLOP/s)', fontsize=YLABEL_SIZE)
    ax[1].margins(y=0.18)
    ax[1].set_xticks(xpos)
    ax[1].set_xticklabels([str(s) for s in SIZES])
    ax[1].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[1].grid(True, axis='y', alpha=.3)

    handles, labels = ax[1].get_legend_handles_labels()  # Cloudburst..WasMem order
    fig.legend(handles, labels, loc='upper center', ncol=2, frameon=False,
               fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.0),
               columnspacing=1.5, handletextpad=0.5)
    fig.tight_layout(rect=[0, 0, 1, 0.82])
    out = os.path.join(FIGS, 'matrix_overlay.pdf')
    fig.savefig(out)
    print('wrote', out)


def bars_fig(systems):
    """One panel per matrix size; each shows the four systems' best end-to-end
    latency (s). Per-panel auto-scaling keeps each size readable on its own scale."""
    fig, ax = plt.subplots(1, len(SIZES), figsize=(11, 4.2))
    if len(SIZES) == 1:
        ax = [ax]
    keys = [k for _, k in systems]                # already legend order
    lat = {k: dict(lat_best_by_size(r, SIZES, k)) for r, k in systems}
    fmt_l = lambda y: ('%.2f' % y) if y < 10 else '%.0f' % y

    for j, s in enumerate(SIZES):
        axis = ax[j]
        ys = [lat[k].get(s, 0) for k in keys]
        bars = axis.bar(range(len(keys)), ys,
                        color=[STYLE[k]['color'] for k in keys])
        for b, y in zip(bars, ys):
            if y:
                axis.text(b.get_x() + b.get_width() / 2, y, fmt_l(y),
                          ha='center', va='bottom', fontsize=11, fontweight='bold')
        axis.set_xlabel('%d×%d' % (s, s), fontsize=LABEL_SIZE)
        axis.set_xticks([])
        axis.margins(y=0.16)
        axis.tick_params(axis='y', labelsize=TICK_SIZE)
        axis.grid(True, axis='y', alpha=.3)
    ax[0].set_ylabel('Latency (s)', fontsize=YLABEL_SIZE)

    handles = [Patch(color=STYLE[k]['color'], label=STYLE[k]['label']) for k in keys]
    fig.legend(handles, [STYLE[k]['label'] for k in keys], loc='upper center',
               ncol=4, frameon=False, fontsize=LEGEND_SIZE,
               bbox_to_anchor=(0.5, 1.0), columnspacing=1.5, handletextpad=0.5)
    fig.tight_layout(rect=[0, 0, 1, 0.90])
    out = os.path.join(FIGS, 'matrix_bars.pdf')
    fig.savefig(out)
    print('wrote', out)


def main():
    ours, cb, rm, fa = load(OURS), load(CB), load(RM), load(FA)
    # Draw/legend order: Cloudburst -> RMMap -> Faasm -> WasMem.
    systems = ((cb, 'cloudburst'), (rm, 'rmmap'), (fa, 'faasm'), (ours, 'ours'))
    overlay_fig(systems)
    bars_fig(systems)


if __name__ == '__main__':
    main()
