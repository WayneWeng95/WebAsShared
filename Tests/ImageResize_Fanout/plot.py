#!/usr/bin/env python3
"""
Plot image-resize / fanout: OUR system vs the Roadrunner-style baselines we ran
on THIS hardware, plus Roadrunner's published transport numbers as reference.

Inputs:
  results.csv (or results_aot.csv)  — OUR system (run.sh)
      size_mb,fanout,topo,func,compute_ms,delivery_ms,worker_ms,throughput_mbps,reps
  baseline/results_baseline.csv     — wasmedge + native baselines (run_baseline.sh)
      size_mb,fanout,mode,total_ms,throughput_mbps,reps
  ../compare_system/roadrunner/.../parallel/intra-node/intra-node-total-throughput.csv
      Fanout,RoadRunner_Embedded,RoadRunner,RunC,Wasmedge   (PUBLISHED, their HW)

Figures (figs/):
  delivery_vs_fanout.pdf       — ours: broadcast/delivery time (flat = zero-copy)
  total_wall_vs_fanout.pdf     — ours vs wasmedge/native (same HW), ms
  throughput_vs_fanout.pdf     — ours vs baselines (same HW) + published RR (dashed)
"""
import csv, pathlib, collections

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
FIGS = HERE / "figs"
PUB = ROOT.parent / "compare_system" / "roadrunner" / "experiments" / "evaluation" \
        / "results" / "parallel" / "intra-node" / "intra-node-total-throughput.csv"

def load_ours(path):
    """-> {fanout: (compute_ms, delivery_ms, worker_ms, mbps)}"""
    out = {}
    if not path.exists():
        return out
    with open(path) as f:
        for r in csv.DictReader(f):
            try:
                out[int(r["fanout"])] = (float(r["compute_ms"]), float(r["delivery_ms"]),
                                         float(r["worker_ms"]), float(r["throughput_mbps"]))
            except (ValueError, KeyError):
                continue
    return out

def load_baseline(path):
    """-> {mode: {fanout: (total_ms, mbps)}}"""
    out = collections.defaultdict(dict)
    if not path.exists():
        return out
    with open(path) as f:
        for r in csv.DictReader(f):
            try:
                out[r["mode"]][int(r["fanout"])] = (float(r["total_ms"]), float(r["throughput_mbps"]))
            except (ValueError, KeyError):
                continue
    return out

def load_pub(path):
    """Published intra-node throughput -> {col: {fanout: value}}"""
    cols = collections.defaultdict(dict)
    if not path.exists():
        return cols
    with open(path) as f:
        for r in csv.DictReader(f):
            try: fo = int(r["Fanout"])
            except (ValueError, KeyError): continue
            for c in ("RoadRunner_Embedded", "RoadRunner", "RunC", "Wasmedge"):
                v = (r.get(c) or "").strip()
                if v:
                    cols[c][fo] = float(v)
    return cols

def main():
    try:
        import matplotlib; matplotlib.use("Agg"); import matplotlib.pyplot as plt
    except ImportError:
        print("matplotlib not installed; skipping plots."); return

    FIGS.mkdir(exist_ok=True)
    ours = load_ours(HERE / "results.csv") or load_ours(HERE / "results_aot.csv")
    base = load_baseline(HERE / "baseline" / "results_baseline.csv")
    pub = load_pub(PUB)
    xo = sorted(ours)

    # 1. Delivery (broadcast) vs fanout — the zero-copy headline.
    if ours:
        fig, ax = plt.subplots()
        ax.plot(xo, [ours[x][1] for x in xo], marker="o", color="C2", label="ours delivery (splice)")
        ax.set_xlabel("fanout degree"); ax.set_ylabel("delivery time (ms)")
        ax.set_title("Zero-copy delivery vs fanout (flat = O(1) one-to-many)")
        ax.legend(fontsize=8); fig.tight_layout(); fig.savefig(FIGS / "delivery_vs_fanout.pdf"); plt.close(fig)

    # 2. Total wall vs fanout — ours vs same-HW baselines (ms).
    fig, ax = plt.subplots()
    if ours: ax.plot(xo, [ours[x][0] for x in xo], marker="o", label="ours (total)")
    if ours: ax.plot(xo, [ours[x][2] for x in xo], marker="o", linestyle=":", label="ours (worker only)")
    for mode, st in (("wasmedge", "-s"), ("native", "-^")):
        if base.get(mode):
            xs = sorted(base[mode]); ax.plot(xs, [base[mode][x][0] for x in xs], st, label=f"{mode} (same HW)")
    ax.set_xlabel("fanout degree"); ax.set_ylabel("wall time (ms)"); ax.set_yscale("log")
    ax.set_title("Fanout wall time — ours vs Roadrunner-style baselines (same HW)")
    ax.legend(fontsize=8); fig.tight_layout(); fig.savefig(FIGS / "total_wall_vs_fanout.pdf"); plt.close(fig)

    # 3. Throughput vs fanout — all on OUR-hardware basis. Roadrunner's published
    #    numbers were taken on hardware ~HW_FACTOR faster than ours (measured from
    #    the single-task wasmedge per-task latency: ours 0.680s / theirs 0.239s ≈ 2.8),
    #    so we scale their throughput DOWN by HW_FACTOR to estimate "RoadRunner on our HW".
    HW_FACTOR = 2.8
    fig, ax = plt.subplots()
    if ours: ax.plot(xo, [ours[x][3] for x in xo], marker="o", label="ours (same HW)")
    for mode in ("wasmedge",):
        if base.get(mode):
            xs = sorted(base[mode]); ax.plot(xs, [base[mode][x][1] for x in xs], marker="s", label=f"{mode} (same HW)")
    for c in ("RoadRunner",):
        if pub.get(c):
            xs = sorted(pub[c]); ax.plot(xs, [pub[c][x] / HW_FACTOR for x in xs],
                                         linestyle="--", marker="x", alpha=.7,
                                         label=f"pub:{c} (÷{HW_FACTOR}, our-HW est.)")
    ax.set_xlabel("fanout degree"); ax.set_ylabel("throughput")
    ax.set_title(f"Throughput vs fanout — our-HW basis (Roadrunner scaled ÷{HW_FACTOR})")
    ax.legend(fontsize=7); fig.tight_layout(); fig.savefig(FIGS / "throughput_vs_fanout.pdf"); plt.close(fig)

    # 4. Per-task wasmedge: OURS vs THEIRS (published), same methodology.
    pt = HERE / "baseline" / "results_pertask.csv"
    pub_lat = ROOT.parent / "compare_system" / "roadrunner" / "experiments" / "evaluation" \
        / "results" / "parallel" / "intra-node" / "intra-node-total-latency.csv"
    if pt.exists():
        ours_pt = {}
        with open(pt) as f:
            for r in csv.DictReader(f):
                if r["mode"] == "wasmedge":
                    try: ours_pt[int(r["fanout"])] = float(r["latency_s"])
                    except ValueError: pass
        theirs = {}
        if pub_lat.exists():
            with open(pub_lat) as f:
                for r in csv.DictReader(f):
                    try:
                        fo = int(r["Fanout"]); v = (r.get("Wasmedge") or "").strip()
                        if v: theirs[fo] = float(v)
                    except (ValueError, KeyError): pass
        fig, ax = plt.subplots()
        xs = sorted(ours_pt); ax.plot(xs, [ours_pt[x] for x in xs], "-o", label="ours (Xeon D-1548, 16c)")
        xt = sorted(theirs); ax.plot(xt, [theirs[x] for x in xt], "--s", alpha=.7, label="theirs (published)")
        ax.set_xlabel("fanout degree"); ax.set_ylabel("per-task latency (s)")
        ax.set_title("Wasmedge per-task latency — our HW vs Roadrunner published")
        ax.legend(fontsize=8); fig.tight_layout(); fig.savefig(FIGS / "wasmedge_pertask_compare.pdf"); plt.close(fig)

    print(f"wrote figs to {FIGS}/")

if __name__ == "__main__":
    main()
