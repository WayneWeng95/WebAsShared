#!/usr/bin/env python3
# plot.py — FINRA cross-system bars: WasMem vs Faasm vs RMMap-ES vs Cloudburst,
# across input size (number of trades). The 8 audit rules are fixed, so the axis
# is trade count (not fan-out). One panel per trade-count, each showing the
# end-to-end latency (ms) of the four systems for that size. Styling matches
# Tests/StateSync.
import csv
import os

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib.patches import Patch

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, 'figs')
os.makedirs(FIGS, exist_ok=True)

TICK_SIZE, LABEL_SIZE, LEGEND_SIZE, YLABEL_SIZE = 16, 16, 17, 16

OURS = os.path.join(HERE, 'results_aot.csv')                         # AOT (fair)
FA = os.path.join(HERE, 'baseline', 'faasm', 'demo', 'results.csv')
RM = os.path.join(HERE, 'baseline', 'rmmap', 'results.csv')
CB = os.path.join(HERE, 'baseline', 'cloudburst', 'results.csv')

# StateSync palette + markers (same per-system assignment as the WordCount plot).
STYLE = {
    'ours':       dict(color='#2057c7', label='WasMem'),
    'faasm':      dict(color='#2a9d8f', label='Faasm'),
    'rmmap':      dict(color='#1d7a3e', label='RMMap'),
    'cloudburst': dict(color='#edae49', label='Cloudburst'),
}
# Latency column per system (ours excludes input staging, the others are e2e).
LAT_KEY = {'ours': 'compute_ms', 'faasm': 'e2e_ms',
           'rmmap': 'e2e_ms', 'cloudburst': 'e2e_ms'}


def load(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [dict(r) for r in csv.DictReader(f)]


def by_size(rows, key):
    return {int(r['size_trades']): float(r[key]) for r in rows if r.get(key)}


def size_label(s):
    return ('%dM trades' % (s // 1000000)) if s >= 1000000 else ('%dk trades' % (s // 1000))


def main():
    data = {'ours': load(OURS), 'faasm': load(FA),
            'rmmap': load(RM), 'cloudburst': load(CB)}
    sizes = sorted({int(r['size_trades']) for rows in data.values() for r in rows})
    order = ['cloudburst', 'rmmap', 'faasm', 'ours']   # bar order = legend order

    # One panel per trade-count; each shows the 4 systems' end-to-end latency (ms).
    fig, ax = plt.subplots(1, len(sizes), figsize=(11, 4.2))
    if len(sizes) == 1:
        ax = [ax]

    for j, s in enumerate(sizes):
        axis = ax[j]
        ys = [by_size(data[k], LAT_KEY[k]).get(s, 0) for k in order]
        bars = axis.bar(range(len(order)), ys,
                        color=[STYLE[k]['color'] for k in order])
        for b, y in zip(bars, ys):
            if y:
                axis.text(b.get_x() + b.get_width() / 2, y, '%.0f' % y,
                          ha='center', va='bottom', fontsize=11, fontweight='bold')
        axis.set_xlabel(size_label(s), fontsize=LABEL_SIZE)
        axis.set_xticks([])
        axis.margins(y=0.16)
        axis.tick_params(axis='y', labelsize=TICK_SIZE)
        axis.grid(True, axis='y', alpha=.3)
    ax[0].set_ylabel('Latency (ms)', fontsize=YLABEL_SIZE)

    handles = [Patch(color=STYLE[k]['color'], label=STYLE[k]['label']) for k in order]
    labels = [STYLE[k]['label'] for k in order]   # Cloudburst…WasMem
    fig.legend(handles, labels, loc='upper center', ncol=4, frameon=False,
               fontsize=LEGEND_SIZE, bbox_to_anchor=(0.5, 1.0),
               columnspacing=1.5, handletextpad=0.5)
    fig.tight_layout(rect=[0, 0, 1, 0.90])
    out = os.path.join(FIGS, 'finra_bars.pdf')
    fig.savefig(out)
    print('wrote', out)


if __name__ == '__main__':
    main()
