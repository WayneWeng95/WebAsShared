#!/usr/bin/env python3
# plot_scaling_bars.py — 1 row x 3 columns of grouped bar charts:
#   (a) 1 node   (b) 4 nodes   (c) 8 nodes
# each panel: the 4 streaming systems x 2 workloads, at that node count.
# Shared log y-axis (systems span ~5k..280k), palette per PLOT_COLOR_SCHEMES.
import csv, os, shutil
import matplotlib; matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__)); FIGS = os.path.join(HERE, 'figs'); os.makedirs(FIGS, exist_ok=True)
TF = os.path.abspath(os.path.join(HERE, '..', '..', 'Figures'))
NODES = [1, 4, 8]; PANEL = {1: '(a) 1 node', 4: '(b) 4 nodes', 8: '(c) 8 nodes'}
WLS = ['mediareview', 'socialnetwork']; WLL = {'mediareview': 'MediaReview', 'socialnetwork': 'SocialNetwork'}
SERIES = [('statefun', 'Flink StateFun', '#d69a60'), ('rtsfaas', 'RTSFaaS*', '#b07ba6'),
          ('wasmem_async', 'WasMem (async)', '#3a5fc4'), ('wasmem_sync', 'WasMem (sync)', '#8ba8e0')]
STAR = {'rtsfaas'}


def rd(p):
    d = {}
    if os.path.exists(p):
        for r in csv.DictReader(open(p)):
            v = r.get('throughput_ev_s', '').strip()
            if v not in ('', 'NA'):
                d[(r.get('system', ''), r['workload'], int(r['nodes']))] = float(v)
    return d

D = {}
for f in ('/tmp/wm_scaling_clean.csv', '/tmp/rts_scaling_clean.csv', '/tmp/sf_scaling.csv'):
    D.update(rd(f))

fig, axes = plt.subplots(1, 3, figsize=(15, 4.4), sharey=True)
nb = len(SERIES); w = 0.8 / nb; x = np.arange(len(WLS))
for ax, N in zip(axes, NODES):
    for i, (key, lab, c) in enumerate(SERIES):
        vals = [(D.get((key, wl, N)) or 0) / 1000.0 for wl in WLS]
        offs = x + (i - (nb - 1) / 2) * w
        bars = ax.bar(offs, vals, w, label=lab, color=c, edgecolor='black', linewidth=0.5)
        for bx, v in zip(offs, vals):
            if v <= 0: continue
            ax.annotate(f'{v:.0f}{"*" if key in STAR else ""}', (bx, v), ha='center', va='bottom',
                        fontsize=9, fontweight='bold', xytext=(0, 1), textcoords='offset points')
    ax.set_yscale('log'); ax.set_title(PANEL[N], fontsize=17, fontweight='bold')
    ax.set_xticks(x); ax.set_xticklabels([WLL[wl] for wl in WLS], fontsize=14)
    ax.grid(axis='y', alpha=0.3, which='both'); ax.set_axisbelow(True)
    ax.set_ylim(3, 400)
    ax.yaxis.set_major_formatter(mticker.FuncFormatter(lambda v, _: f'{v:.0f}'))
    ax.tick_params(axis='y', labelsize=13)
axes[0].set_ylabel('Throughput (k req/s)', fontsize=16)
h, l = axes[0].get_legend_handles_labels()
fig.legend(h, l, ncol=4, fontsize=14, loc='lower center', bbox_to_anchor=(0.5, 0.985),
           frameon=False, columnspacing=1.4, handletextpad=0.5)
fig.tight_layout(rect=[0, 0, 1, 0.93])
for ext in ('png', 'pdf'):
    out = os.path.join(FIGS, f'cluster_throughput_scaling_bars.{ext}')
    fig.savefig(out, dpi=150, bbox_inches='tight'); print('wrote', out)
    if ext == 'pdf': shutil.copy(out, os.path.join(TF, 'cluster_throughput_scaling_bars.pdf'))
