#!/usr/bin/env python3
# plot_9node.py — INTER-NODE (9-node cluster) streaming throughput overlay.
# Same style as plot.py (StateSync palette, grouped bars, two-level broken y-axis)
# but with the 9-node results and the axis rescaled for the larger throughputs.
#
# Series: WasMem (async), WasMem (sync) — peak wall throughput @3.6M events, 9 nodes;
#         RTSFaaS* — load-balanced parallel estimate (sum of 9 independent nodes);
#         Flink StateFun — filled in once deployed (16/node, 1 vCPU/2 GB pods).
# Writes figs/cluster_throughput_overlay_9node.{png,pdf} (+ Tests/Figures/).
import os, shutil
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, 'figs'); os.makedirs(FIGS, exist_ok=True)
TESTS_FIGS = os.path.abspath(os.path.join(HERE, '..', '..', 'Figures'))

TICK_SIZE = 16; LABEL_SIZE = 17; LEGEND_SIZE = 13; YLABEL_SIZE = 17
WORKLOADS = ['mediareview', 'socialnetwork']
WL_LABEL = {'mediareview': 'MediaReview', 'socialnetwork': 'SocialNetwork'}

# 9-node throughput (events/records per sec). StateFun pending → omitted (None).
DATA = {
    'statefun':   {'mediareview': 16537, 'socialnetwork': 11204},  # 128 pods 16/node 1cpu/2G FT-on
    'rtsfaas':    {'mediareview': 45422,  'socialnetwork': 51834},    # load-balanced estimate
    'was_async':  {'mediareview': 306304, 'socialnetwork': 265663},   # persist=async @3.6M
    'was_sync':   {'mediareview': 248842, 'socialnetwork': 229416},   # persist=sync  @3.6M
}
SERIES = [
    ('statefun',  'Flink StateFun',    '#d69a60'),
    ('rtsfaas',   'RTSFaaS*',          '#b07ba6'),
    ('was_async', 'WasMem (async)',    '#3a5fc4'),
    ('was_sync',  'WasMem (sync)',     '#8ba8e0'),
]
STAR = {'rtsfaas'}

# two-level broken y-axis (k req/s): lower band = RTSFaaS (~45-52k); upper = WasMem (~229-306k)
LOW_TOP = 70.0
HIGH_BOT, HIGH_TOP = 200.0, 340.0


def main():
    n = len(SERIES); x = np.arange(len(WORKLOADS)); width = 0.8 / n
    fig, (ax_hi, ax_lo) = plt.subplots(
        2, 1, sharex=True, figsize=(9, 4.5),
        gridspec_kw={'height_ratios': [2.2, 1], 'hspace': 0.06})

    for i, (key, label, color) in enumerate(SERIES):
        vals = [ (DATA[key].get(w) or 0) / 1000.0 for w in WORKLOADS ]
        offs = x + (i - (n - 1) / 2) * width
        ax_hi.bar(offs, vals, width, label=label, color=color, edgecolor='black', linewidth=0.5)
        ax_lo.bar(offs, vals, width, color=color, edgecolor='black', linewidth=0.5)
        suffix = '*' if key in STAR else ''
        for b_x, v in zip(offs, vals):
            if v <= 0:   # missing (StateFun) — no label
                continue
            ax = ax_hi if v >= LOW_TOP else ax_lo
            ax.annotate(f'{v:.0f}{suffix}', (b_x, v), ha='center', va='bottom',
                        fontsize=11, fontweight='bold', xytext=(0, 2), textcoords='offset points')

    ax_hi.set_ylim(HIGH_BOT, HIGH_TOP); ax_lo.set_ylim(0, LOW_TOP)
    for ax in (ax_hi, ax_lo):
        ax.tick_params(axis='y', labelsize=TICK_SIZE); ax.grid(axis='y', alpha=0.3)
        ax.set_axisbelow(True)
        ax.yaxis.set_major_formatter(mticker.FuncFormatter(lambda v, _: f'{v:.0f}'))
    ax_hi.spines['bottom'].set_visible(False); ax_lo.spines['top'].set_visible(False)
    ax_hi.tick_params(axis='x', bottom=False)
    d = 0.5
    kw = dict(marker=[(-1, -d), (1, d)], markersize=12, linestyle='none',
              color='k', mec='k', mew=1, clip_on=False)
    ax_hi.plot([0, 1], [0, 0], transform=ax_hi.transAxes, **kw)
    ax_lo.plot([0, 1], [1, 1], transform=ax_lo.transAxes, **kw)

    ax_lo.set_xticks(x); ax_lo.set_xticklabels([WL_LABEL[w] for w in WORKLOADS], fontsize=LABEL_SIZE)
    ax_lo.set_xlim(x[0] - 0.5, x[-1] + 0.5)
    fig.supylabel('Throughput (k requests / sec)', fontsize=YLABEL_SIZE, x=0.04)
    handles, labels = ax_hi.get_legend_handles_labels()
    fig.legend(handles, labels, ncol=len(SERIES), fontsize=LEGEND_SIZE,
               loc='lower center', bbox_to_anchor=(0.5, 0.99), bbox_transform=ax_hi.transAxes,
               frameon=False, columnspacing=0.9, handletextpad=0.4, handlelength=1.4)

    for ext in ('png', 'pdf'):
        out = os.path.join(FIGS, f'cluster_throughput_overlay_9node.{ext}')
        fig.savefig(out, dpi=150, bbox_inches='tight', pad_inches=0.02)
        print('wrote', out)
        if ext == 'pdf':
            os.makedirs(TESTS_FIGS, exist_ok=True)
            shutil.copy(out, os.path.join(TESTS_FIGS, 'cluster_throughput_overlay_9node.pdf'))
    for key, label, _ in SERIES:
        print(f'  {label:16s} ' + '  '.join(f'{w}={DATA[key].get(w)}' for w in WORKLOADS))


if __name__ == '__main__':
    main()
