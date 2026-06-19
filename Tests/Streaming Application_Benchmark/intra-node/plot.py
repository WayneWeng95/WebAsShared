#!/usr/bin/env python3
# plot.py — streaming-application throughput overlay: WasMem (ours) vs RTSFaaS vs
# Flink StateFun, for both workloads (MediaReview, SocialNetwork).
#
# Grouped bars, throughput in requests/sec (= events/sec; fixed-batch max-rate,
# single machine, 8-core budget). WasMem is shown WITH fault tolerance in two
# modes — async offload (durable by end of run) and sync barrier (durable before
# the next stage) — while RTSFaaS / Flink StateFun are FT-off. The spread is
# ~1.5k..84k with nothing in between, so instead of a log axis we use a two-level
# broken (split) linear y-axis: a lower band (0..8k) for the RTSFaaS / StateFun
# baselines and an upper band (30k..95k) for the WasMem bars.
#
# Reads the measured CSVs:
#   results_was_ft_modes.csv      (ft_mode = async / sync_barrier)  -> WasMem
#   results_common_comparison.csv (system  = rtsfaas / statefun)    -> baselines
# Writes figs/streaming_overlay.{png,pdf}. Same StateSync palette + font sizes as
# the intra/inter-node application-benchmark plots.
import csv
import os

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, 'figs')
os.makedirs(FIGS, exist_ok=True)

# Font sizes matched to the application-benchmark figures (plot_mem_largest.py).
TICK_SIZE = 16
LABEL_SIZE = 17
LEGEND_SIZE = 17
YLABEL_SIZE = 17

WORKLOADS = ['mediareview', 'socialnetwork']
WL_LABEL = {'mediareview': 'MediaReview', 'socialnetwork': 'SocialNetwork'}

# StateSync palette: ours = #2057c7 (WasMem blue); sync barrier a lighter blue so
# the two WasMem modes group visually; baselines reuse the teal/amber slots.
SERIES = [
    ('statefun',  'Flink StateFun',         '#e8843c'),   # orange
    ('rtsfaas',   'RTSFaaS',                '#2a9d8f'),   # teal (matches latency_throughput_remote.pdf)
    ('was_sync',  'WasMem (sync)',  '#7aa5e0'),
    ('was_async', 'WasMem (async)', '#2057c7'),
]

data = {k: {} for k, _, _ in SERIES}


def load():
    with open(os.path.join(HERE, 'results_was_ft_modes.csv')) as f:
        for r in csv.DictReader(f):
            if r['ft_mode'] == 'async':
                data['was_async'][r['workload']] = float(r['throughput_req_s'])
            elif r['ft_mode'] == 'sync_barrier':
                data['was_sync'][r['workload']] = float(r['throughput_req_s'])
    with open(os.path.join(HERE, 'results_common_comparison.csv')) as f:
        for r in csv.DictReader(f):
            if r['system'] in ('rtsfaas', 'statefun'):
                data[r['system']][r['workload']] = float(r['throughput_req_s'])


# Two-level broken y-axis. Upper band holds the WasMem bars (all >= 35k); lower
# band holds the RTSFaaS / StateFun baselines (all <= 6.2k). The empty 8k..30k gap
# is cut out. LOW_TOP also = where a value is annotated on the lower vs upper axis.
LOW_TOP = 8000.0          # top of the lower band
HIGH_BOT, HIGH_TOP = 30000.0, 95000.0   # bounds of the upper band


def main():
    load()
    n = len(SERIES)
    x = np.arange(len(WORKLOADS))
    width = 0.8 / n

    # Two stacked axes sharing the x-axis; the upper band gets more height since
    # it carries the main story (the WasMem bars).
    fig, (ax_hi, ax_lo) = plt.subplots(
        2, 1, sharex=True, figsize=(9, 4.5),
        gridspec_kw={'height_ratios': [2.2, 1], 'hspace': 0.06})

    for i, (key, label, color) in enumerate(SERIES):
        vals = [data[key].get(w, 0) for w in WORKLOADS]
        offs = x + (i - (n - 1) / 2) * width
        # Same bars on both axes; each axis clips to its own band.
        ax_hi.bar(offs, vals, width, label=label, color=color,
                  edgecolor='black', linewidth=0.5)
        ax_lo.bar(offs, vals, width, color=color,
                  edgecolor='black', linewidth=0.5)
        for b_x, v in zip(offs, vals):
            ax = ax_hi if v >= LOW_TOP else ax_lo
            ax.annotate(f'{v:,.0f}', (b_x, v), ha='center', va='bottom',
                        fontsize=10, fontweight='bold',
                        xytext=(0, 2), textcoords='offset points')

    ax_hi.set_ylim(HIGH_BOT, HIGH_TOP)
    ax_lo.set_ylim(0, LOW_TOP)
    for ax in (ax_hi, ax_lo):
        ax.tick_params(axis='y', labelsize=TICK_SIZE)
        ax.grid(axis='y', alpha=0.3)
        ax.set_axisbelow(True)
        ax.yaxis.set_major_formatter(
            mticker.FuncFormatter(lambda v, _: f'{v:,.0f}'))

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
    # Tighten the side margins so the bars sit closer to the left/right borders.
    ax_lo.set_xlim(x[0] - 0.5, x[-1] + 0.5)
    # Centre the shared y-label vertically across both bands and pull it clear of
    # the (wide) tick numbers.
    fig.supylabel('Throughput (requests / sec)', fontsize=YLABEL_SIZE, x=-0.01)

    # Legend as a single horizontal row centered just above the plot (matches the
    # mem_largest_load layout). Anchored to the top axes so the gap to the plot is
    # small and fixed; tightened columns/handles to keep it compact.
    handles, labels = ax_hi.get_legend_handles_labels()
    fig.legend(handles, labels, ncol=4, fontsize=LEGEND_SIZE - 5,
               loc='lower center', bbox_to_anchor=(0.5, 0.99),
               bbox_transform=ax_hi.transAxes,
               frameon=False, columnspacing=0.9, handletextpad=0.4,
               handlelength=1.4)

    # No tight_layout (incompatible with the broken-axis supylabel); savefig's
    # bbox_inches='tight' already crops to include the top legend.
    for ext in ('png', 'pdf'):
        out = os.path.join(FIGS, f'streaming_overlay.{ext}')
        fig.savefig(out, dpi=150, bbox_inches='tight', pad_inches=0.02)
        print('wrote', out)


if __name__ == '__main__':
    main()
