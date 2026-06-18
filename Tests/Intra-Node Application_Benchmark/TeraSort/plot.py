#!/usr/bin/env python3
# plot.py — TeraSort cross-system overlay: WebAsShared (ours) vs Cloudburst vs
# RMMap-ES vs Faasm-like, across input size {50, 500 MB} and shuffle fan-out N.
#
# TeraSort moves the ENTIRE dataset across an N x N shuffle, so the zero-copy-vs-
# serialized gap is the largest of the suite. Three panels:
#   (1) Throughput vs N at 500 MB — parallel-scaling shape per system.
#   (2) Peak throughput per input size — grouped bars (50 / 500 MB).
#   (3) Shuffle bytes through the external store (x input): the headline —
#       ours = 0 (zero-copy page-chain splice), baselines ~1x..4x (every record
#       serialized through the KV).
#
# Robust to CRASH rows (ours, low-N / 1 GB OOM) and partial sweeps.
import csv
import os

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, 'figs')
os.makedirs(FIGS, exist_ok=True)

TICK_SIZE = 15
LABEL_SIZE = 15
LEGEND_SIZE = 16
YLABEL_SIZE = 15

OURS = os.path.join(HERE, 'results_aot.csv')
CB = os.path.join(HERE, 'baseline', 'cloudburst', 'results.csv')
RM = os.path.join(HERE, 'baseline', 'rmmap', 'results.csv')
FA = os.path.join(HERE, 'baseline', 'faasm', 'demo', 'results.csv')

# Same palette as the other workload plots (StateSync assignment).
STYLE = {
    'ours':       dict(color='#2057c7', marker='o', label='WasMem'),
    'faasm':      dict(color='#2a9d8f', marker='D', label='Faasm'),
    'rmmap':      dict(color='#1d7a3e', marker='*', label='RMMap'),
    'cloudburst': dict(color='#edae49', marker='v', label='Cloudburst'),
}
SIZES = [50, 250, 500]  # unified MB axis across all four systems


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


def tput_vs_n(rows, size):
    pts = []
    for r in rows:
        if abs((fnum(r['size_mb']) or 0) - size) > 1.5:
            continue
        t = fnum(r.get('throughput_mb_s'))
        if t is not None:
            pts.append((int(float(r['workers'])), t))
    return sorted(pts)


def best_tput_by_size(rows, sizes):
    out = {}
    for s in sizes:
        ts = [fnum(r.get('throughput_mb_s')) for r in rows
              if abs((fnum(r['size_mb']) or 0) - s) <= 1.5
              and fnum(r.get('throughput_mb_s'))]
        if ts:
            out[s] = max(ts)
    return out


def shuffle_amplification(rows, sizes, system):
    """x input bytes moved through the external KV, averaged over N per size."""
    out = {}
    for s in sizes:
        vals = []
        for r in rows:
            if abs((fnum(r['size_mb']) or 0) - s) > 1.5:
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
            elif system == 'faasm':
                k = fnum(r.get('state_kv_mb'))
                if k is not None:
                    vals.append(k / s)
        if vals:
            out[s] = sum(vals) / len(vals)
    return out


def main():
    systems = [(load(OURS), 'ours'), (load(CB), 'cloudburst'),
               (load(RM), 'rmmap'), (load(FA), 'faasm')]
    order = ['cloudburst', 'rmmap', 'faasm', 'ours']
    rows_by = {k: r for r, k in systems}

    fig, ax = plt.subplots(1, 3, figsize=(16, 4.6))

    # ── Panel 1: throughput vs N at 500 MB ────────────────────────────────────
    for k in order:
        pts = [(n, t) for n, t in tput_vs_n(rows_by[k], 500) if n >= 2]
        if pts:
            xs, ys = zip(*pts)
            ax[0].plot(xs, ys, **STYLE[k])
    ax[0].set_title('Throughput vs fan-out N  (500 MB)', fontsize=LABEL_SIZE)
    ax[0].set_xlabel('shuffle workers N', fontsize=LABEL_SIZE)
    ax[0].set_ylabel('throughput (MB/s)', fontsize=YLABEL_SIZE)
    ax[0].set_xscale('log', base=2)
    ax[0].set_xticks([2, 4, 8, 16])
    ax[0].get_xaxis().set_major_formatter(matplotlib.ticker.ScalarFormatter())
    ax[0].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[0].grid(True, alpha=.3)

    # ── Panel 2: peak throughput per input size (grouped bars) ────────────────
    width = 0.2
    xpos = list(range(len(SIZES)))
    for i, k in enumerate(order):
        bysize = best_tput_by_size(rows_by[k], SIZES)
        ys = [bysize.get(s, 0) for s in SIZES]
        bars = ax[1].bar([x + (i - 1.5) * width for x in xpos], ys, width,
                         color=STYLE[k]['color'], label=STYLE[k]['label'])
        for b, y in zip(bars, ys):
            if y:
                ax[1].text(b.get_x() + b.get_width() / 2, y, '%.0f' % y,
                           ha='center', va='bottom', fontsize=10, fontweight='bold')
    ax[1].set_title('Peak throughput per input size', fontsize=LABEL_SIZE)
    ax[1].set_xlabel('input size (MB)', fontsize=LABEL_SIZE)
    ax[1].set_ylabel('throughput (MB/s)', fontsize=YLABEL_SIZE)
    ax[1].set_xticks(xpos)
    ax[1].set_xticklabels([str(s) for s in SIZES])
    ax[1].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[1].grid(True, axis='y', alpha=.3)

    # ── Panel 3: shuffle bytes through the external store (x input) ───────────
    for i, k in enumerate(order):
        amp = shuffle_amplification(rows_by[k], SIZES, k)
        ys = [amp.get(s, 0) for s in SIZES]
        bars = ax[2].bar([x + (i - 1.5) * width for x in xpos], ys, width,
                         color=STYLE[k]['color'], label=STYLE[k]['label'])
        for b, y in zip(bars, ys):
            lbl = '0' if (k == 'ours') else ('%.1f' % y if y else '')
            if lbl:
                ax[2].text(b.get_x() + b.get_width() / 2, y, lbl + '×',
                           ha='center', va='bottom', fontsize=10, fontweight='bold')
    ax[2].set_title('Shuffle bytes through external store', fontsize=LABEL_SIZE)
    ax[2].set_xlabel('input size (MB)', fontsize=LABEL_SIZE)
    ax[2].set_ylabel('data moved (× input)', fontsize=YLABEL_SIZE)
    ax[2].set_xticks(xpos)
    ax[2].set_xticklabels([str(s) for s in SIZES])
    ax[2].tick_params(axis='both', labelsize=TICK_SIZE)
    ax[2].grid(True, axis='y', alpha=.3)

    handles, labels = ax[1].get_legend_handles_labels()
    fig.legend(handles, labels, loc='upper center', ncol=4, frameon=False,
               fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.0),
               columnspacing=2.0, handletextpad=0.5)
    fig.tight_layout(rect=[0, 0, 1, 0.9])
    out = os.path.join(FIGS, 'terasort_overlay.pdf')
    fig.savefig(out)
    print('wrote', out)


if __name__ == '__main__':
    main()
