#!/usr/bin/env python3
# plot.py — streaming-application throughput overlay: WasMem (ours) vs RTSFaaS vs
# Flink StateFun, for both workloads (MediaReview, SocialNetwork).
#
# Grouped bars, throughput in requests/sec (= events/sec; fixed-batch max-rate,
# single machine, 8-core budget). WasMem is shown WITH fault tolerance in two
# modes — async offload (durable by end of run) and sync barrier (durable before
# the next stage) — while RTSFaaS / Flink StateFun are FT-off. Log y (spread ~1.5k..84k).
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
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
FIGS = os.path.join(HERE, 'figs')
os.makedirs(FIGS, exist_ok=True)

TICK_SIZE = 15
LABEL_SIZE = 15
LEGEND_SIZE = 16
YLABEL_SIZE = 15

WORKLOADS = ['mediareview', 'socialnetwork']
WL_LABEL = {'mediareview': 'MediaReview', 'socialnetwork': 'SocialNetwork'}

# StateSync palette: ours = #2057c7 (WasMem blue); sync barrier a lighter blue so
# the two WasMem modes group visually; baselines reuse the teal/amber slots.
SERIES = [
    ('was_async', 'WasMem (async offload)', '#2057c7'),
    ('was_sync',  'WasMem (sync barrier)',  '#7aa5e0'),
    ('rtsfaas',   'RTSFaaS',                '#edae49'),
    ('statefun',  'Flink StateFun',         '#2a9d8f'),
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


def main():
    load()
    n = len(SERIES)
    x = np.arange(len(WORKLOADS))
    width = 0.8 / n

    fig, ax = plt.subplots(figsize=(9, 5.5))
    for i, (key, label, color) in enumerate(SERIES):
        vals = [data[key].get(w, 0) for w in WORKLOADS]
        offs = x + (i - (n - 1) / 2) * width
        bars = ax.bar(offs, vals, width, label=label, color=color,
                      edgecolor='black', linewidth=0.5)
        for b, v in zip(bars, vals):
            ax.annotate(f'{v:,.0f}', (b.get_x() + b.get_width() / 2, v),
                        ha='center', va='bottom', fontsize=11,
                        xytext=(0, 2), textcoords='offset points')

    ax.set_yscale('log')
    ax.set_ylabel('Throughput (requests / sec)', fontsize=YLABEL_SIZE)
    ax.set_xticks(x)
    ax.set_xticklabels([WL_LABEL[w] for w in WORKLOADS], fontsize=LABEL_SIZE)
    ax.tick_params(axis='y', labelsize=TICK_SIZE)
    ax.legend(frameon=False, fontsize=LEGEND_SIZE - 4, ncol=2, loc='upper right')
    ax.grid(axis='y', which='both', color='#cccccc', linestyle='--',
            linewidth=0.5, alpha=0.7)
    ax.set_axisbelow(True)
    ax.set_ylim(top=max(v for d in data.values() for v in d.values()) * 2.2)

    fig.tight_layout()
    for ext in ('png', 'pdf'):
        out = os.path.join(FIGS, f'streaming_overlay.{ext}')
        fig.savefig(out, dpi=150, bbox_inches='tight')
        print('wrote', out)


if __name__ == '__main__':
    main()
