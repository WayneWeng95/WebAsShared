#!/usr/bin/env python3
# plot.py — INTER-NODE (4-node cluster) streaming throughput overlay: WasMem (ours)
# vs RTSFaaS, for both workloads (MediaReview, SocialNetwork). Mirrors the intra-node
# figure (intra-node/plot.py): same StateSync palette, fonts, grouped bars, and a
# two-level broken (split) linear y-axis to span the ~10x gap between the baselines
# and WasMem.
#
# Bars (peak sustained throughput on 4 nodes):
#   WasMem (sync)  / WasMem (async)  — peak throughput_req_s over the event-count sweep
#                                      (persist=sync/async), from results_persist_sweep_*.csv
#   RTSFaaS*       — the embarrassingly-parallel ESTIMATE (sum of 4 independent
#                    single-node runs; see baseline/RTSFaaS/cluster/rt-parallel-estimate.py
#                    + results_rtsfaas_parallel_estimate.csv). The * marks that this is
#                    a parallel UPPER BOUND, not a coordinated multi-node run
#                    (coordinated: SocialNetwork ~1x / 5,867; MediaReview hangs).
#                    The caption explains the *.
#
# Writes figs/cluster_throughput_overlay.{png,pdf}.
import csv
import os
import shutil

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, 'figs')
os.makedirs(FIGS, exist_ok=True)
# Per PLOT_COLOR_SCHEMES.md: streaming/bar scripts write to a local figs/ and a copy
# is collected in Tests/Figures/ (HERE -> inter-node -> Streaming.../.. -> Tests).
TESTS_FIGS = os.path.abspath(os.path.join(HERE, '..', '..', 'Figures'))

# Font sizes matched to the application-benchmark figures (same as intra-node/plot.py).
TICK_SIZE = 16
LABEL_SIZE = 17
LEGEND_SIZE = 17
YLABEL_SIZE = 17

WORKLOADS = ['mediareview', 'socialnetwork']
WL_LABEL = {'mediareview': 'MediaReview', 'socialnetwork': 'SocialNetwork'}

# Same palette as intra-node/plot.py. RTSFaaS keeps its mauve; WasMem sync/async the
# light/dark blue pair. (No Flink StateFun bar — not run inter-node.)
SERIES = [
    ('rtsfaas',   'RTSFaaS*',        '#b07ba6'),   # mauve — the * = parallel estimate
    ('was_sync',  'WasMem (sync)',   '#8aa8f0'),   # WasMem light blue
    ('was_async', 'WasMem (async)',  '#3a5fc4'),   # WasMem blue (ours, pop)
]
STAR = {'rtsfaas'}   # series whose value labels get a trailing '*'

data = {k: {} for k, _, _ in SERIES}


def load():
    # WasMem: peak throughput_req_s over the successful event-count sweep, per FT mode.
    for w in WORKLOADS:
        path = os.path.join(HERE, f'results_persist_sweep_{w}.csv')
        peak = {'sync': 0.0, 'async': 0.0}
        with open(path) as f:
            for r in csv.DictReader(f):
                if r['success'].strip().lower() != 'true':
                    continue
                m = r['persist'].strip()
                if m in peak:
                    peak[m] = max(peak[m], float(r['throughput_req_s']))
        data['was_sync'][w] = peak['sync']
        data['was_async'][w] = peak['async']
    # RTSFaaS: the parallel estimate (sum of 4 independent single-node runs).
    with open(os.path.join(HERE, 'baseline/RTSFaaS/cluster/results_rtsfaas_parallel_estimate.csv')) as f:
        for r in csv.DictReader(f):
            data['rtsfaas'][r['workload']] = float(r['parallel_estimate_req_s'])


# Two-level broken y-axis, in k requests/sec. Upper band holds the WasMem bars
# (~92..128k); lower band holds the RTSFaaS estimate (~20..23k). The empty 30..80k
# gap is cut out. (All limits/values are in thousands.)
LOW_TOP = 30.0                     # top of the lower band  (k req/s)
HIGH_BOT, HIGH_TOP = 80.0, 140.0   # bounds of the upper band (k req/s)


def main():
    load()
    n = len(SERIES)
    x = np.arange(len(WORKLOADS))
    width = 0.8 / n

    fig, (ax_hi, ax_lo) = plt.subplots(
        2, 1, sharex=True, figsize=(9, 4.5),
        gridspec_kw={'height_ratios': [2.2, 1], 'hspace': 0.06})

    for i, (key, label, color) in enumerate(SERIES):
        vals = [data[key].get(w, 0) / 1000.0 for w in WORKLOADS]   # -> k req/s
        offs = x + (i - (n - 1) / 2) * width
        ax_hi.bar(offs, vals, width, label=label, color=color,
                  edgecolor='black', linewidth=0.5)
        ax_lo.bar(offs, vals, width, color=color,
                  edgecolor='black', linewidth=0.5)
        suffix = '*' if key in STAR else ''
        for b_x, v in zip(offs, vals):
            ax = ax_hi if v >= LOW_TOP else ax_lo
            ax.annotate(f'{v:.1f}{suffix}', (b_x, v), ha='center', va='bottom',
                        fontsize=11, fontweight='bold',
                        xytext=(0, 2), textcoords='offset points')

    ax_hi.set_ylim(HIGH_BOT, HIGH_TOP)
    ax_lo.set_ylim(0, LOW_TOP)
    for ax in (ax_hi, ax_lo):
        ax.tick_params(axis='y', labelsize=TICK_SIZE)
        ax.grid(axis='y', alpha=0.3)
        ax.set_axisbelow(True)
        ax.yaxis.set_major_formatter(
            mticker.FuncFormatter(lambda v, _: f'{v:.0f}'))

    # Hide the spines/ticks at the break and draw the diagonal break marks.
    ax_hi.spines['bottom'].set_visible(False)
    ax_lo.spines['top'].set_visible(False)
    ax_hi.tick_params(axis='x', bottom=False)
    d = 0.5
    kw = dict(marker=[(-1, -d), (1, d)], markersize=12, linestyle='none',
              color='k', mec='k', mew=1, clip_on=False)
    ax_hi.plot([0, 1], [0, 0], transform=ax_hi.transAxes, **kw)
    ax_lo.plot([0, 1], [1, 1], transform=ax_lo.transAxes, **kw)

    ax_lo.set_xticks(x)
    ax_lo.set_xticklabels([WL_LABEL[w] for w in WORKLOADS], fontsize=LABEL_SIZE)
    ax_lo.set_xlim(x[0] - 0.5, x[-1] + 0.5)
    # k-scale tick labels are short (<=3 digits), so pull the y-label in close.
    fig.supylabel('Throughput (k requests / sec)', fontsize=YLABEL_SIZE, x=0.04)

    handles, labels = ax_hi.get_legend_handles_labels()
    fig.legend(handles, labels, ncol=len(SERIES), fontsize=LEGEND_SIZE - 4,
               loc='lower center', bbox_to_anchor=(0.5, 0.99),
               bbox_transform=ax_hi.transAxes,
               frameon=False, columnspacing=0.9, handletextpad=0.4,
               handlelength=1.4)

    os.makedirs(TESTS_FIGS, exist_ok=True)
    for ext in ('png', 'pdf'):
        out = os.path.join(FIGS, f'cluster_throughput_overlay.{ext}')
        fig.savefig(out, dpi=150, bbox_inches='tight', pad_inches=0.02)
        print('wrote', out)
        if ext == 'pdf':   # Tests/Figures collects PDFs only (png stays local for preview)
            copy = os.path.join(TESTS_FIGS, 'cluster_throughput_overlay.pdf')
            shutil.copy(out, copy)
            print('  copied ->', copy)
    # echo the plotted values for the record
    for key, label, _ in SERIES:
        print(f'  {label:16s} ' + '  '.join(f'{w}={data[key].get(w,0):,.0f}' for w in WORKLOADS))


if __name__ == '__main__':
    main()
