#!/usr/bin/env python3
# plot.py — MNIST-inference cross-system overlay: WebAsShared (ours) vs Cloudburst
# (single-process Redis-KVS dataflow) vs RMMap-ES (parallel pods over Redis) vs
# Faasm-like (WASM Faaslets over Redis), across dataset size and gradient-worker
# fan-out W. All systems run the SAME integer SGD kernel (sgd_core == the guest),
# so the comparison is about the data substrate — per-iteration model/gradient/
# data state moving zero-copy through SHM (ours) vs serialized through Redis
# (baselines) — not kernel speed. Every system reproduces the weight-checksum gate.
#
# Emits (styling matched to ../Matrix/plot.py and Tests/StateSync):
#   ml_inference_overlay.pdf : (1) throughput (M predictions/s) vs W at the largest
#                             size  (2) peak throughput per size bars
#   ml_inference_bars.pdf    : the paper figure — one panel per size, four systems'
#                             best end-to-end inference latency (s), bars in legend order.
#
# Robust to CRASH rows and partial sweeps. ours = the AOT line (results_aot.csv);
# point OURS at results.csv for the JIT reference.
import csv
import os

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.ticker
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
    'ours':       dict(color='#2057c7', marker='o', label='WasMem'),
    'faasm':      dict(color='#2a9d8f', marker='D', label='Faasm'),
    'rmmap':      dict(color='#1d7a3e', marker='*', label='RMMap'),
    'cloudburst': dict(color='#edae49', marker='v', label='Cloudburst'),
}

LAT_KEY = 'compute_ms'          # every CSV shares the same e2e-training column
THR_KEY = 'samples_per_s'

# Dataset-size panels, keyed by size_mb (≈ sample count); labels for the x-axis.
SIZE_LABEL = [(3.2, '100k'), (9.7, '300k'), (19.5, '600k')]


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


def _match(r, size_mb):
    return abs((fnum(r.get('size_mb')) or -1) - size_mb) <= 1.0


def thr_vs_w(rows, size_mb):
    """(W, M predictions/s) for a size, sorted by W."""
    pts = []
    for r in rows:
        if not _match(r, size_mb):
            continue
        t = fnum(r.get(THR_KEY))
        if t is None:
            continue
        pts.append((int(float(r['workers'])), t / 1e6))
    return sorted(pts)


def best_thr_by_size(rows):
    """(size_mb, max M predictions/s over W) — skips CRASH/missing cells."""
    out = []
    for s, _ in SIZE_LABEL:
        ts = [fnum(r.get(THR_KEY)) for r in rows if _match(r, s) and fnum(r.get(THR_KEY))]
        if ts:
            out.append((s, max(ts) / 1e6))
    return out


def lat_best_by_size(rows):
    """(size_mb, MIN e2e latency in seconds over W) — best config per size."""
    out = []
    for s, _ in SIZE_LABEL:
        vs = [fnum(r.get(LAT_KEY)) for r in rows if _match(r, s) and fnum(r.get(LAT_KEY))]
        if vs:
            out.append((s, min(vs) / 1000.0))
    return out


def overlay_fig(systems):
    fig, ax = plt.subplots(1, 2, figsize=(9, 4.5))
    width = 0.2
    sizes = [s for s, _ in SIZE_LABEL]
    xpos = list(range(len(sizes)))
    big = sizes[-1]

    # Panel 1: throughput vs W at the largest size (higher is better).
    for rows, key in systems:
        pts = [(w, t) for w, t in thr_vs_w(rows, big) if w >= 1]
        if pts:
            xs, ys = zip(*pts)
            ax[0].plot(xs, ys, **STYLE[key])
    ax[0].set_title('Throughput vs workers W  (%s)' % SIZE_LABEL[-1][1], fontsize=LABEL_SIZE)
    ax[0].set_xlabel('predict workers W', fontsize=LABEL_SIZE)
    ax[0].set_ylabel('M predictions/s', fontsize=YLABEL_SIZE)
    ax[0].set_xscale('log', base=2)
    ax[0].set_xticks([1, 2, 4, 8, 16])
    ax[0].get_xaxis().set_major_formatter(matplotlib.ticker.ScalarFormatter())
    ax[0].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[0].grid(True, alpha=.3)

    # Panel 2: peak throughput per dataset size — grouped bars.
    for i, (rows, key) in enumerate(systems):
        bysize = dict(best_thr_by_size(rows))
        ys = [bysize.get(s, 0) for s in sizes]
        ax[1].bar([x + (i - 1.5) * width for x in xpos], ys, width,
                  color=STYLE[key]['color'], label=STYLE[key]['label'])
        for k, y in enumerate(ys):
            if y:
                ax[1].text(xpos[k] + (i - 1.5) * width, y, '%.1f' % y,
                           ha='center', va='bottom', fontsize=9, fontweight='bold')
    ax[1].set_title('Peak throughput per size', fontsize=LABEL_SIZE)
    ax[1].set_xlabel('dataset (samples)', fontsize=LABEL_SIZE)
    ax[1].set_ylabel('M predictions/s', fontsize=YLABEL_SIZE)
    ax[1].margins(y=0.18)
    ax[1].set_xticks(xpos)
    ax[1].set_xticklabels([lbl for _, lbl in SIZE_LABEL])
    ax[1].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[1].grid(True, axis='y', alpha=.3)

    handles, labels = ax[1].get_legend_handles_labels()  # Cloudburst..WasMem order
    fig.legend(handles, labels, loc='upper center', ncol=2, frameon=False,
               fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.0),
               columnspacing=1.5, handletextpad=0.5)
    fig.tight_layout(rect=[0, 0, 1, 0.82])
    out = os.path.join(FIGS, 'ml_inference_overlay.pdf')
    fig.savefig(out)
    print('wrote', out)


def bars_fig(systems):
    """One panel per dataset size; each shows the four systems' best end-to-end
    inference latency (s). Per-panel auto-scaling keeps each size readable."""
    fig, ax = plt.subplots(1, len(SIZE_LABEL), figsize=(11, 4.2))
    if len(SIZE_LABEL) == 1:
        ax = [ax]
    keys = [k for _, k in systems]                # already legend order
    lat = {k: dict(lat_best_by_size(r)) for r, k in systems}
    fmt_l = lambda y: ('%.2f' % y) if y < 10 else '%.0f' % y

    for j, (s, lbl) in enumerate(SIZE_LABEL):
        axis = ax[j]
        ys = [lat[k].get(s, 0) for k in keys]
        bars = axis.bar(range(len(keys)), ys,
                        color=[STYLE[k]['color'] for k in keys])
        for b, y in zip(bars, ys):
            if y:
                axis.text(b.get_x() + b.get_width() / 2, y, fmt_l(y),
                          ha='center', va='bottom', fontsize=11, fontweight='bold')
        axis.set_xlabel('%s samples' % lbl, fontsize=LABEL_SIZE)
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
    out = os.path.join(FIGS, 'ml_inference_bars.pdf')
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
