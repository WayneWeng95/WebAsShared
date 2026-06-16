#!/usr/bin/env python3
# plot.py — WordCount cross-system overlay: WebAsShared (ours) vs Cloudburst vs
# RMMap-ES, across corpus size {50, 500, 1000 MB} and map fan-out N.
#
# Reads the three results.csv files (different schemas) and emits one figure:
#   (1) Throughput vs N at 500 MB (N>=2) — parallel-scaling shape per system
#   (2) Peak throughput per corpus size  — grouped bars (50/500/1000 MB)
#
# Robust to CRASH rows (ours, low-N OOM) and partial sweeps.
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

OURS = os.path.join(HERE, 'results_aot.csv')        # AOT guest (fair vs Faasm's AOT WASM)
OURS_JIT = os.path.join(HERE, 'results.csv')        # JIT guest (reference)
CB = os.path.join(HERE, 'baseline', 'cloudburst', 'results.csv')
RM = os.path.join(HERE, 'baseline', 'rmmap', 'results.csv')
FA = os.path.join(HERE, 'baseline', 'faasm', 'demo', 'results.csv')

# Colors + markers matched to Tests/StateSync/plot_compare.py, which assigns
# these same systems explicitly (its LABEL/COLOR/MARKER dicts):
#   WasMem→engine blue, Faasm→shm-copy teal, RMMap→shm-zerocopy green,
#   Cloudburst→redis-local amber.
STYLE = {
    'ours':       dict(color='#2057c7', marker='o', label='WasMem'),      # engine blue
    'faasm':      dict(color='#2a9d8f', marker='D', label='Faasm'),       # shm-copy teal
    'rmmap':      dict(color='#1d7a3e', marker='*', label='RMMap'),       # shm-zerocopy green
    'cloudburst': dict(color='#edae49', marker='v', label='Cloudburst'),  # redis amber
}

# Per-system end-to-end latency column (different names across the CSVs).
LAT_KEY = {'ours': 'compute_ms', 'faasm': 'e2e_ms',
           'rmmap': 'e2e_ms', 'cloudburst': 'e2e_ms_median'}


def fnum(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None


def load(path):
    """Return list of dict rows with floats where possible."""
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [dict(r) for r in csv.DictReader(f)]


def series_tput_vs_n(rows, size, tput_key='throughput_mb_s'):
    """(N, throughput) points for a given corpus size, sorted by N."""
    pts = []
    for r in rows:
        if abs(fnum(r['size_mb']) - size) > 1.5:
            continue
        t = fnum(r.get(tput_key))
        if t is None:
            continue
        pts.append((int(float(r['workers'])), t))
    return sorted(pts)


def best_tput_by_size(rows, sizes, tput_key='throughput_mb_s'):
    """(size, max throughput over N) — skips CRASH/missing cells."""
    out = []
    for s in sizes:
        ts = [fnum(r.get(tput_key)) for r in rows
              if abs(fnum(r['size_mb']) - s) <= 1.5 and fnum(r.get(tput_key))]
        if ts:
            out.append((s, max(ts)))
    return out


def amplification(rows, sizes, system):
    """× corpus bytes moved through an external KVS, per size (avg over N)."""
    out = []
    for s in sizes:
        vals = []
        for r in rows:
            if abs(fnum(r['size_mb']) - s) > 1.5:
                continue
            if system == 'ours':
                vals.append(0.0)
            elif system == 'cloudburst':
                put, get = fnum(r.get('kvs_put_mb')), fnum(r.get('kvs_get_mb'))
                reps = fnum(r.get('reps')) or 1
                if put is not None and get is not None:
                    vals.append((put + get) / reps / s)
            elif system == 'rmmap':
                k = fnum(r.get('kvs_ser_mb'))
                if k is not None:
                    vals.append(k / s)
        if vals:
            out.append((s, sum(vals) / len(vals)))
    return out


def lat_series_vs_n(rows, size, key):
    """(N, e2e latency in seconds) for a corpus size, sorted by N."""
    pts = []
    for r in rows:
        if abs(fnum(r['size_mb']) - size) > 1.5:
            continue
        v = fnum(r.get(LAT_KEY[key]))
        if v is None:
            continue
        pts.append((int(float(r['workers'])), v / 1000.0))
    return sorted(pts)


def lat_best_by_size(rows, sizes, key):
    """(size, MIN e2e latency in seconds over N) — best config per size."""
    out = []
    for s in sizes:
        vs = [fnum(r.get(LAT_KEY[key])) for r in rows
              if abs(fnum(r['size_mb']) - s) <= 1.5 and fnum(r.get(LAT_KEY[key]))]
        if vs:
            out.append((s, min(vs) / 1000.0))
    return out


def top_legend(fig, ax, ncol=2):
    handles, labels = ax.get_legend_handles_labels()   # already Cloudburst…WasMem
    fig.legend(handles, labels, loc='upper center', ncol=ncol, frameon=False,
               fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.0),
               columnspacing=1.5, handletextpad=0.5)


def latency_fig(systems, sizes):
    """End-to-end latency: vs fan-out N at 500 MB, and best per corpus size."""
    fig, ax = plt.subplots(1, 2, figsize=(9, 4.5))
    width = 0.2
    xpos = list(range(len(sizes)))

    # Panel 1: latency vs N at 500 MB (lower is better).
    for rows, key in systems:
        pts = [(n, t) for n, t in lat_series_vs_n(rows, 500, key) if n >= 2]
        if pts:
            xs, ys = zip(*pts)
            ax[0].plot(xs, ys, **STYLE[key])
    ax[0].set_title('End-to-end latency vs fan-out N  (500 MB)', fontsize=LABEL_SIZE)
    ax[0].set_xlabel('map workers N', fontsize=LABEL_SIZE)
    ax[0].set_ylabel('e2e latency (s)', fontsize=YLABEL_SIZE)
    ax[0].set_xscale('log', base=2)
    ax[0].set_xticks([2, 4, 8, 16])
    ax[0].get_xaxis().set_major_formatter(matplotlib.ticker.ScalarFormatter())
    ax[0].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[0].grid(True, alpha=.3)

    # Panel 2: best (min) latency per corpus size — log y (spans ~0.5–100 s).
    for i, (rows, key) in enumerate(systems):
        bysize = dict(lat_best_by_size(rows, sizes, key))
        ys = [bysize.get(s, 0) for s in sizes]
        bars = ax[1].bar([x + (i - 1.5) * width for x in xpos], ys, width,
                         color=STYLE[key]['color'], label=STYLE[key]['label'])
        for b, y in zip(bars, ys):
            if y:
                ax[1].text(b.get_x() + b.get_width() / 2, y,
                           ('%.1f' % y) if y < 10 else '%.0f' % y,
                           ha='center', va='bottom', fontsize=10, fontweight='bold')
    ax[1].set_title('Best end-to-end latency per size', fontsize=LABEL_SIZE)
    ax[1].set_xlabel('corpus size (MB)', fontsize=LABEL_SIZE)
    ax[1].set_ylabel('e2e latency (s, log)', fontsize=YLABEL_SIZE)
    ax[1].set_yscale('log')
    ax[1].set_xticks(xpos)
    ax[1].set_xticklabels([str(s) for s in sizes])
    ax[1].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[1].grid(True, axis='y', which='both', alpha=.3)

    top_legend(fig, ax[0])
    fig.tight_layout(rect=[0, 0, 1, 0.82])
    out = os.path.join(FIGS, 'wordcount_latency.pdf')
    fig.savefig(out)
    print('wrote', out)


def bars_fig(systems, sizes):
    """One panel per corpus size; each shows the four systems' best end-to-end
    latency (s) for that size. Per-panel auto-scaling makes the 104 s outlier at
    1000 MB readable without a broken axis."""
    fig, ax = plt.subplots(1, len(sizes), figsize=(11, 4.2))
    if len(sizes) == 1:
        ax = [ax]
    keys = [k for _, k in systems]                # already legend order
    lat = {k: dict(lat_best_by_size(r, sizes, k)) for r, k in systems}
    fmt_l = lambda y: ('%.1f' % y) if y < 10 else '%.0f' % y

    for j, s in enumerate(sizes):
        axis = ax[j]
        ys = [lat[k].get(s, 0) for k in keys]
        bars = axis.bar(range(len(keys)), ys,
                        color=[STYLE[k]['color'] for k in keys])
        for b, y in zip(bars, ys):
            if y:
                axis.text(b.get_x() + b.get_width() / 2, y, fmt_l(y),
                          ha='center', va='bottom', fontsize=11, fontweight='bold')
        axis.set_xlabel('%d MB' % s, fontsize=LABEL_SIZE)
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
    out = os.path.join(FIGS, 'wordcount_bars.pdf')
    fig.savefig(out)
    print('wrote', out)


def main():
    ours, cb, rm, fa = load(OURS), load(CB), load(RM), load(FA)
    sizes = [50, 500, 1000]

    fig, ax = plt.subplots(1, 2, figsize=(9, 4.5))
    width = 0.2
    xpos = list(range(len(sizes)))

    # ── Panel 1: throughput vs N at 500 MB (drop N=1) ─────────────────────────
    for rows, key in ((cb, 'cloudburst'), (rm, 'rmmap'), (fa, 'faasm'), (ours, 'ours')):
        pts = [(n, t) for n, t in series_tput_vs_n(rows, 500) if n >= 2]
        if pts:
            xs, ys = zip(*pts)
            ax[0].plot(xs, ys, **STYLE[key])
    ax[0].set_title('Throughput vs fan-out N  (500 MB corpus)', fontsize=LABEL_SIZE)
    ax[0].set_xlabel('map workers N', fontsize=LABEL_SIZE)
    ax[0].set_ylabel('Throughput (MB/s)', fontsize=YLABEL_SIZE)
    ax[0].set_xscale('log', base=2)
    ax[0].set_xticks([2, 4, 8, 16])
    ax[0].get_xaxis().set_major_formatter(matplotlib.ticker.ScalarFormatter())
    ax[0].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[0].grid(True, alpha=.3)

    # ── Panel 2: peak throughput per corpus size — grouped bars ───────────────
    order = ((cb, 'cloudburst'), (rm, 'rmmap'), (fa, 'faasm'), (ours, 'ours'))
    for i, (rows, key) in enumerate(order):
        bysize = dict(best_tput_by_size(rows, sizes))
        ys = [bysize.get(s, 0) for s in sizes]
        bars = ax[1].bar([x + (i - 1.5) * width for x in xpos], ys, width,
                         color=STYLE[key]['color'], label=STYLE[key]['label'])
        for b, y in zip(bars, ys):
            ax[1].text(b.get_x() + b.get_width() / 2, y, '%.0f' % y,
                       ha='center', va='bottom', fontsize=11, fontweight='bold')
    ax[1].set_title('Peak throughput per corpus size', fontsize=LABEL_SIZE)
    ax[1].set_xlabel('corpus size (MB)', fontsize=LABEL_SIZE)
    ax[1].set_ylabel('Throughput (MB/s)', fontsize=YLABEL_SIZE)
    ax[1].margins(y=0.18)  # headroom so bar labels clear the legend
    ax[1].set_xticks(xpos)
    ax[1].set_xticklabels([str(s) for s in sizes])
    ax[1].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[1].grid(True, axis='y', alpha=.3)

    # Single 2x2 legend at the top, no frame (Cloudburst…WasMem order, matching
    # the bar group order — handles already come in that order).
    handles, labels = ax[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc='upper center', ncol=2, frameon=False,
               fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.0),
               columnspacing=1.5, handletextpad=0.5)
    fig.tight_layout(rect=[0, 0, 1, 0.82])
    out = os.path.join(FIGS, 'wordcount_overlay.pdf')
    fig.savefig(out)  # vector PDF
    print('wrote', out)

    systems = ((cb, 'cloudburst'), (rm, 'rmmap'), (fa, 'faasm'), (ours, 'ours'))
    latency_fig(systems, sizes)
    bars_fig(systems, sizes)


if __name__ == '__main__':
    main()
