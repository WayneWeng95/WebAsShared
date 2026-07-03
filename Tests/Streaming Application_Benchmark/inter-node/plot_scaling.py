#!/usr/bin/env python3
# plot_scaling.py — throughput vs node count (1,4,8) for the 3 streaming systems,
# both workloads. Two panels; log-y (systems span ~11k..190k). Palette per PLOT_COLOR_SCHEMES.
import csv, os, shutil
import matplotlib; matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np
HERE=os.path.dirname(os.path.abspath(__file__)); FIGS=os.path.join(HERE,'figs'); os.makedirs(FIGS,exist_ok=True)
TF=os.path.abspath(os.path.join(HERE,'..','..','Figures'))
NODES=[1,4,8]; WLS=['mediareview','socialnetwork']; WLL={'mediareview':'MediaReview','socialnetwork':'SocialNetwork'}
def rd(p):
    d={}
    if os.path.exists(p):
        for r in csv.DictReader(open(p)):
            if r.get('throughput_ev_s','').strip() not in ('','NA'):
                d[(r.get('system',''),r['workload'],int(r['nodes']))]=float(r['throughput_ev_s'])
    return d
D={}; D.update(rd('/tmp/wm_scaling_clean.csv')); D.update(rd('/tmp/rts_scaling_clean.csv')); D.update(rd('/tmp/sf_scaling.csv'))
SERIES=[('wasmem_async','WasMem (async)','#3a5fc4','o'),('wasmem_sync','WasMem (sync)','#8ba8e0','s'),
        ('rtsfaas','RTSFaaS*','#b07ba6','^'),('statefun','Flink StateFun','#d69a60','D')]
fig,axes=plt.subplots(1,2,figsize=(11,4.3),sharey=True)
for ax,wl in zip(axes,WLS):
    for key,lab,c,m in SERIES:
        ys=[D.get((key,wl,n)) for n in NODES]
        xs=[n for n,y in zip(NODES,ys) if y]; ys=[y/1000 for y in ys if y]
        if ys: ax.plot(xs,ys,marker=m,color=c,label=lab,linewidth=2,markersize=8)
    ax.set_xscale('log',base=2); ax.set_yscale('log'); ax.set_xticks(NODES); ax.set_xticklabels(NODES,fontsize=14)
    ax.set_title(WLL[wl],fontsize=16,fontweight='bold'); ax.set_xlabel('Nodes',fontsize=15); ax.grid(alpha=.3,which='both')
    ax.tick_params(axis='y',labelsize=13)
axes[0].set_ylabel('Throughput (k req/s)',fontsize=15)
h,l=axes[0].get_legend_handles_labels()
fig.legend(h,l,ncol=4,fontsize=13,loc='lower center',bbox_to_anchor=(0.5,0.99),bbox_transform=axes[0].transAxes,frameon=False)
fig.tight_layout(rect=[0,0,1,0.94])
for ext in ('png','pdf'):
    out=os.path.join(FIGS,f'cluster_throughput_scaling.{ext}'); fig.savefig(out,dpi=150,bbox_inches='tight')
    print('wrote',out)
    if ext=='pdf': shutil.copy(out,os.path.join(TF,'cluster_throughput_scaling.pdf'))
for k,l2,_,_ in SERIES:
    print(f'  {l2:16s} '+'  '.join(f'{wl[:2]}{n}={D.get((k,wl,n))}' for wl in WLS for n in NODES))
