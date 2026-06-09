#!/usr/bin/env python3
"""Plot multi-node RDMA bandwidth results from `rdma_bw` (the bandwidth companion
to rdma_latency).

CSV schema (written by `cargo run --example rdma_bw -- send ... --csv bw.csv`):
    tag,n_links,size_bytes,iters,per_link_gibps,agg_gibps

Each `tag` is one topology run, e.g.:
    pair       1 link   A -> B                 (single-link line-rate)
    fanout3    3 links  A -> {B,C,D}           (sender egress-port shared)
    incast     1 link   each of A,C,D -> B     (sum across senders = ingress cap)
    pairs2     1 link   A->B and C->D at once  (disjoint pairs; aggregate scales)

Renders (into ../../Graph):
    bandwidth_remote.<fmt>       aggregate GiB/s vs state size, one line per tag,
                                 with the 10 GbE per-port ceiling drawn for ref.
    bandwidth_per_link.<fmt>     per-link GiB/s vs size (shows fan-out's 1/N split)

Usage:
    python3 plot_bw.py                       # reads bw.csv in cwd
    python3 plot_bw.py --csv bw.csv --format pdf
"""

import argparse
import csv
import os
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

TICK_SIZE, LABEL_SIZE, LEGEND_SIZE, YLABEL_SIZE = 16, 16, 15, 15
plt.rcParams.update({
    "xtick.labelsize": TICK_SIZE, "ytick.labelsize": TICK_SIZE,
    "axes.labelsize": LABEL_SIZE, "legend.fontsize": LEGEND_SIZE,
})

# 10 GbE per-port wire ceiling (see StateSync-remote README hardware note):
#   10 Gb/s / 8 / 1024^3 ≈ 1.16 GiB/s raw; ~1.13 usable after RoCE overhead.
PORT_CEILING_GIBPS = 10e9 / 8 / (1024 ** 3)

# Stable colours; unknown tags fall back to the cycle.
COLOR = {
    "pair":    "#2a9d8f",
    "pairs2":  "#1d7a3e",
    "pairs3":  "#145a2c",
    "fanout2": "#e8843c",
    "fanout3": "#d1495b",
    "incast":  "#5b8fb9",
}
MARKER = {"pair": "*", "pairs2": "P", "pairs3": "X",
          "fanout2": "^", "fanout3": "v", "incast": "o"}


def fmt_size(n):
    n = int(n)
    if n < 1024: return f"{n}B"
    if n < 1024 ** 2: return f"{n // 1024}KiB"
    if n < 1024 ** 3: return f"{n // 1024 ** 2}MiB"
    return f"{n // 1024 ** 3}GiB"


def load(path):
    """Return {tag: [(size, n_links, per_link, agg), ...]} sorted by size."""
    data = defaultdict(list)
    with open(path) as fh:
        for r in csv.DictReader(fh):
            data[r["tag"]].append((int(r["size_bytes"]), int(r["n_links"]),
                                   float(r["per_link_gibps"]), float(r["agg_gibps"])))
    for t in data:
        data[t].sort(key=lambda x: x[0])
    return data


def style(tag):
    return dict(color=COLOR.get(tag), marker=MARKER.get(tag, "o"),
                linewidth=1.9, markersize=9, label=tag)


def plot_metric(data, col, ylabel, outpath, figsize, ceiling=True):
    all_sizes = sorted({s for t in data for s, *_ in data[t]})
    idx = {s: i for i, s in enumerate(all_sizes)}
    fig, ax = plt.subplots(figsize=figsize)
    for tag in sorted(data):
        xs = [idx[s] for s, *_ in data[tag]]
        ys = [row[col] for row in data[tag]]
        ax.plot(xs, ys, **style(tag))
    if ceiling:
        ax.axhline(PORT_CEILING_GIBPS, ls="--", color="#888", lw=1.4)
        ax.text(0.02, PORT_CEILING_GIBPS * 1.03, "10 GbE port ceiling",
                color="#666", fontsize=12, transform=ax.get_yaxis_transform())
    ax.set_xticks(range(len(all_sizes)))
    ax.set_xticklabels([fmt_size(s) for s in all_sizes])
    ax.set_xlabel("state size")
    ax.set_ylabel(ylabel, fontsize=YLABEL_SIZE)
    ax.grid(True, which="both", ls=":", alpha=0.4)
    ax.legend(ncol=2, frameon=False)
    fig.tight_layout()
    fig.savefig(outpath, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot_bw] wrote {outpath}")


def main():
    ap = argparse.ArgumentParser(description="Plot rdma_bw multi-node bandwidth")
    ap.add_argument("--csv", default="bw.csv")
    ap.add_argument("--outdir", default=os.path.normpath(os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", "Graph")))
    ap.add_argument("--format", default="pdf", choices=["png", "pdf", "svg"])
    ap.add_argument("--figsize", default="9,3.2")
    args = ap.parse_args()

    if not os.path.exists(args.csv):
        raise SystemExit(f"no CSV at {args.csv} (run rdma_bw send ... --csv {args.csv})")
    data = load(args.csv)
    if not data:
        raise SystemExit(f"no rows in {args.csv}")
    figsize = tuple(float(x) for x in args.figsize.split(","))
    os.makedirs(args.outdir, exist_ok=True)
    ext = args.format

    plot_metric(data, 3, "Aggregate bandwidth (GiB/s)",
                os.path.join(args.outdir, f"bandwidth_remote.{ext}"), figsize)
    plot_metric(data, 2, "Per-link bandwidth (GiB/s)",
                os.path.join(args.outdir, f"bandwidth_per_link.{ext}"), figsize)

    # stdout summary
    print(f"\n{'tag':>10} {'links':>6} {'size':>8} {'per-link':>10} {'aggregate':>11}")
    for tag in sorted(data):
        for s, n, pl, agg in data[tag]:
            print(f"{tag:>10} {n:>6} {fmt_size(s):>8} {pl:>9.3f}  {agg:>10.3f}")


if __name__ == "__main__":
    main()
