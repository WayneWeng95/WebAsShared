#!/usr/bin/env python3
"""
Plot image-resize / fanout results: OUR system vs Roadrunner.

Our numbers: Tests/ImageResize_Fanout/results.csv  (written by run.sh)
    columns: size_mb,fanout,topo,total_ms,throughput_mbps,rss_bytes,reps

Roadrunner numbers (optional overlay): pulled from
    ../compare_system/roadrunner/experiments/evaluation/results/parallel/inter-node/
        inter-node-fanout-total_throughput.csv
        inter-node-fanout-total_latency.csv
    columns: fanout,roadrunner,runc,wasmedge

Produces figs/throughput_vs_fanout.pdf and figs/latency_vs_fanout.pdf.

Usage:  python3 Tests/ImageResize_Fanout/plot.py
"""
import csv, pathlib, collections

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
FIGS = HERE / "figs"
RR = ROOT.parent / "compare_system" / "roadrunner" / "experiments" / "evaluation" \
        / "results" / "parallel" / "inter-node"

def load_ours(path):
    """-> {size_mb: {fanout: (total_ms, mbps, delivery_ms)}}"""
    out = collections.defaultdict(dict)
    if not path.exists():
        return out
    with open(path) as f:
        for r in csv.DictReader(f):
            try:
                ms = float(r["compute_ms"]); tp = float(r["throughput_mbps"])
            except (ValueError, KeyError):
                continue
            try:
                dl = float(r.get("delivery_ms", "nan"))
            except ValueError:
                dl = float("nan")
            out[r["size_mb"]][int(r["fanout"])] = (ms, tp, dl)
    return out

def load_rr(path):
    """Roadrunner CSV -> {col: {fanout: value}} for cols roadrunner/runc/wasmedge."""
    cols = collections.defaultdict(dict)
    if not path.exists():
        return cols
    with open(path) as f:
        for r in csv.DictReader(f):
            fo = int(r["fanout"])
            for c in ("roadrunner", "runc", "wasmedge"):
                v = r.get(c, "").strip()
                if v:
                    cols[c][fo] = float(v)
    return cols

def main():
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("matplotlib not installed; skipping plots. `pip install matplotlib`")
        return

    FIGS.mkdir(exist_ok=True)
    ours = load_ours(HERE / "results.csv")
    rr_tp = load_rr(RR / "inter-node-fanout-total_throughput.csv")
    rr_lat = load_rr(RR / "inter-node-fanout-total_latency.csv")

    # Throughput vs fanout
    fig, ax = plt.subplots()
    for size, by_fo in sorted(ours.items(), key=lambda kv: float(kv[0])):
        xs = sorted(by_fo)
        ax.plot(xs, [by_fo[x][1] for x in xs], marker="o", label=f"ours {size}MB")
    for c, by_fo in rr_tp.items():
        xs = sorted(by_fo)
        ax.plot(xs, [by_fo[x] for x in xs], marker="s", linestyle="--", label=f"rr:{c}")
    ax.set_xlabel("fanout degree"); ax.set_ylabel("throughput")
    ax.set_title("Image-resize / fanout — throughput vs fanout")
    ax.legend(fontsize=7); fig.tight_layout()
    fig.savefig(FIGS / "throughput_vs_fanout.pdf"); plt.close(fig)

    # Latency vs fanout
    fig, ax = plt.subplots()
    for size, by_fo in sorted(ours.items(), key=lambda kv: float(kv[0])):
        xs = sorted(by_fo)
        ax.plot(xs, [by_fo[x][0] for x in xs], marker="o", label=f"ours {size}MB")
    for c, by_fo in rr_lat.items():
        xs = sorted(by_fo)
        ax.plot(xs, [by_fo[x] for x in xs], marker="s", linestyle="--", label=f"rr:{c}")
    ax.set_xlabel("fanout degree"); ax.set_ylabel("latency")
    ax.set_title("Image-resize / fanout — latency vs fanout")
    ax.legend(fontsize=7); fig.tight_layout()
    fig.savefig(FIGS / "latency_vs_fanout.pdf"); plt.close(fig)

    # Headline: zero-copy delivery (broadcast wave) vs fanout — should be ~flat.
    fig, ax = plt.subplots()
    for size, by_fo in sorted(ours.items(), key=lambda kv: float(kv[0])):
        xs = sorted(by_fo)
        ax.plot(xs, [by_fo[x][2] for x in xs], marker="o", label=f"ours delivery {size}MB")
    ax.set_xlabel("fanout degree"); ax.set_ylabel("delivery time (ms)")
    ax.set_title("Zero-copy delivery vs fanout (flat = O(1) one-to-many)")
    ax.legend(fontsize=7); fig.tight_layout()
    fig.savefig(FIGS / "delivery_vs_fanout.pdf"); plt.close(fig)

    print(f"wrote {FIGS}/throughput_vs_fanout.pdf, latency_vs_fanout.pdf, delivery_vs_fanout.pdf")

if __name__ == "__main__":
    main()
