#!/usr/bin/env python3
# plot_throughput_combined.py — streaming throughput, single-node (intra) and
# 4-node cluster (inter) SIDE BY SIDE in one 1x2 figure (figsize 9x4.5), shared
# legend, y in k requests/sec. Mirrors the two standalone figures
# (intra-node/plot.py + inter-node/plot.py) and the unified palette
# (Tests/PLOT_COLOR_SCHEMES.md). Each column keeps its OWN two-level broken y-axis:
# the single-node baselines (~1.5..6 k) and the cluster baseline RTSFaaS* (~20..23 k)
# need different lower-band zooms, so the columns share the UNIT (k req/s) but not
# the scale — which is fine, they're different throughput regimes anyway.
#
# Bars: Flink StateFun, RTSFaaS, WasMem (sync), WasMem (async). StateFun is now run
# on both (single node + 4-node cluster). The cluster RTSFaaS bars are the
# embarrassingly-parallel ESTIMATE and carry a '*' (caption explains it).
#
# Writes figs/throughput_combined.{png,pdf}; copies the PDF to Tests/Figures/.
import csv
import os
import shutil

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.patches as mpatches
import matplotlib.ticker as mticker
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, 'figs'); os.makedirs(FIGS, exist_ok=True)
TESTS_FIGS = os.path.abspath(os.path.join(HERE, '..', 'Figures'))   # HERE -> Streaming.../.. == Tests

TICK_SIZE, LABEL_SIZE, LEGEND_SIZE, YLABEL_SIZE, TITLE_SIZE = 13, 15, 13, 16, 15

WORKLOADS = ['mediareview', 'socialnetwork']
WL_LABEL = {'mediareview': 'MediaReview', 'socialnetwork': 'SocialNetwork'}

# Unified palette (Tests/PLOT_COLOR_SCHEMES.md). Legend order = draw order.
COLOR = {'statefun': '#d69a60', 'rtsfaas': '#b07ba6',
         'was_sync': '#8aa8f0', 'was_async': '#3a5fc4'}
LEGEND = [('statefun', 'Flink StateFun'), ('rtsfaas', 'RTSFaaS'),
          ('was_sync', 'WasMem (sync)'), ('was_async', 'WasMem (async)')]


def load_intra():
    d = {k: {} for k in COLOR}
    with open(os.path.join(HERE, 'intra-node', 'results_was_ft_modes.csv')) as f:
        for r in csv.DictReader(f):
            if r['ft_mode'] == 'async':
                d['was_async'][r['workload']] = float(r['throughput_req_s'])
            elif r['ft_mode'] == 'sync_barrier':
                d['was_sync'][r['workload']] = float(r['throughput_req_s'])
    with open(os.path.join(HERE, 'intra-node', 'results_common_comparison.csv')) as f:
        for r in csv.DictReader(f):
            if r['system'] in ('rtsfaas', 'statefun'):
                d[r['system']][r['workload']] = float(r['throughput_req_s'])
    return d


def load_inter():
    d = {k: {} for k in COLOR}
    for w in WORKLOADS:                       # WasMem: peak over the successful sweep, per FT mode
        peak = {'sync': 0.0, 'async': 0.0}
        with open(os.path.join(HERE, 'inter-node', f'results_persist_sweep_{w}.csv')) as f:
            for r in csv.DictReader(f):
                if r['success'].strip().lower() == 'true' and r['persist'].strip() in peak:
                    peak[r['persist'].strip()] = max(peak[r['persist'].strip()], float(r['throughput_req_s']))
        d['was_sync'][w], d['was_async'][w] = peak['sync'], peak['async']
    with open(os.path.join(HERE, 'inter-node', 'baseline/RTSFaaS/cluster/results_rtsfaas_parallel_estimate.csv')) as f:
        for r in csv.DictReader(f):
            d['rtsfaas'][r['workload']] = float(r['parallel_estimate_req_s'])
    # Flink StateFun: peak achieved events/s over the 4-node throughput ramps
    # (base 24x1 + scaled 3x16), from inter-node/baseline/FlinkStateFun/.
    for name in ('results_throughput_statefun.csv', 'results_throughput_statefun_scaled.csv'):
        with open(os.path.join(HERE, 'inter-node', 'baseline/FlinkStateFun', name)) as f:
            for r in csv.DictReader(f):
                w = r['workload']
                d['statefun'][w] = max(d['statefun'].get(w, 0.0), float(r['achieved_ev_s']))
    return d


def draw_panel(ax_hi, ax_lo, data, series, bands, title, star_keys=()):
    """One broken-axis grouped-bar panel (values shown in k req/s)."""
    low_top, hi_bot, hi_top = bands
    x = np.arange(len(WORKLOADS)); n = len(series); width = 0.8 / n
    for i, key in enumerate(series):
        vals = [data[key].get(w, 0) / 1000.0 for w in WORKLOADS]
        offs = x + (i - (n - 1) / 2) * width
        for ax in (ax_hi, ax_lo):
            ax.bar(offs, vals, width, color=COLOR[key], edgecolor='black', linewidth=0.5)
        suffix = '*' if key in star_keys else ''
        for bx, v in zip(offs, vals):
            ax = ax_hi if v >= low_top else ax_lo
            ann = ax.annotate(f'{v:.1f}{suffix}', (bx, v), ha='center', va='bottom',
                              fontsize=9, fontweight='bold', xytext=(0, 2),
                              textcoords='offset points')
            ann.set_clip_on(True)   # keep each label inside its own band (no stray text at the break)
    ax_hi.set_ylim(hi_bot, hi_top); ax_lo.set_ylim(0, low_top)
    for ax in (ax_hi, ax_lo):
        ax.tick_params(axis='y', labelsize=TICK_SIZE)
        ax.grid(axis='y', alpha=0.3); ax.set_axisbelow(True)
        ax.yaxis.set_major_formatter(mticker.FuncFormatter(lambda v, _: f'{v:.0f}'))
    ax_hi.spines['bottom'].set_visible(False); ax_lo.spines['top'].set_visible(False)
    # hide the upper axis's OWN x tick labels (no sharex here): they were the stray
    # "0.00 0.25 ..." numbers that showed up as a line right at the break.
    ax_hi.tick_params(axis='x', bottom=False, labelbottom=False)
    d = 0.5
    kw = dict(marker=[(-1, -d), (1, d)], markersize=10, linestyle='none',
              color='k', mec='k', mew=1, clip_on=False)
    ax_hi.plot([0, 1], [0, 0], transform=ax_hi.transAxes, **kw)
    ax_lo.plot([0, 1], [1, 1], transform=ax_lo.transAxes, **kw)
    ax_lo.set_xticks(x); ax_lo.set_xticklabels([WL_LABEL[w] for w in WORKLOADS], fontsize=LABEL_SIZE)
    ax_lo.set_xlim(x[0] - 0.5, x[-1] + 0.5)
    # panel label UNDER the column (below the workload names)
    ax_lo.set_xlabel(title, fontsize=TITLE_SIZE, labelpad=10)


def main():
    intra, inter = load_intra(), load_inter()
    fig = plt.figure(figsize=(9, 4.5))
    # explicit margins so the legend can sit right above the bars (small top gap)
    gs = fig.add_gridspec(2, 2, height_ratios=[2.2, 1], hspace=0.07, wspace=0.20,
                          left=0.10, right=0.985, top=0.90, bottom=0.16)
    # column 0 = single-node, column 1 = 4-node cluster; each is a broken axis.
    a_hi, a_lo = fig.add_subplot(gs[0, 0]), fig.add_subplot(gs[1, 0])
    b_hi, b_lo = fig.add_subplot(gs[0, 1]), fig.add_subplot(gs[1, 1])
    # bands = (lower-top, upper-bottom, upper-top)
    draw_panel(a_hi, a_lo, intra, ['statefun', 'rtsfaas', 'was_sync', 'was_async'],
               (8.0, 30.0, 95.0), '(a) Single node')
    draw_panel(b_hi, b_lo, inter, ['statefun', 'rtsfaas', 'was_sync', 'was_async'],
               (30.0, 80.0, 140.0), '(b) 4-node cluster', star_keys={'rtsfaas'})

    fig.supylabel('Throughput (k requests / sec)', fontsize=YLABEL_SIZE, x=0.040)
    handles = [mpatches.Patch(facecolor=COLOR[k], edgecolor='black', linewidth=0.5, label=lab)
               for k, lab in LEGEND]
    fig.legend(handles=handles, ncol=len(LEGEND), fontsize=LEGEND_SIZE,
               loc='lower center', bbox_to_anchor=(0.5, 0.905),   # just above the axes top (0.90)
               frameon=False, columnspacing=1.1, handletextpad=0.4, handlelength=1.4)

    for ext in ('png', 'pdf'):
        out = os.path.join(FIGS, f'throughput_combined.{ext}')
        fig.savefig(out, dpi=150, bbox_inches='tight', pad_inches=0.02)
        print('wrote', out)
        if ext == 'pdf':                      # Tests/Figures collects PDFs only
            os.makedirs(TESTS_FIGS, exist_ok=True)
            dst = os.path.join(TESTS_FIGS, 'throughput_combined.pdf')
            shutil.copy(out, dst); print('  copied ->', dst)


if __name__ == '__main__':
    main()
