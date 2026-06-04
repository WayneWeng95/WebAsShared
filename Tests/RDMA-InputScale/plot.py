#!/usr/bin/env python3
"""Plot the 2-node RDMA word-count input-size sweep (results.csv).

Renders figs/rdma_input_scale.png — per-node + total wall time vs input size,
showing node 1 (receiver + global reducer) on the critical path above node 0.
"""
import argparse, csv, os
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))

TICK, LABEL, LEG = 15, 15, 14
plt.rcParams.update({"xtick.labelsize": TICK, "ytick.labelsize": TICK,
                     "axes.labelsize": LABEL, "legend.fontsize": LEG})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", default=os.path.join(HERE, "results.csv"))
    ap.add_argument("--outdir", default=os.path.normpath(os.path.join(HERE, "..", "Graph")))
    ap.add_argument("--format", default="pdf", choices=["png", "pdf", "svg"])
    args = ap.parse_args()

    rows = sorted((r for r in csv.DictReader(open(args.csv))), key=lambda r: int(r["size_mb"]))
    if not rows:
        raise SystemExit("no rows in results.csv")
    mb = [int(r["size_mb"]) for r in rows]
    xs = list(range(len(mb)))
    n0 = [float(r["node0_ms"]) for r in rows]
    n1 = [float(r["node1_ms"]) for r in rows]
    tot = [float(r["total_ms"]) for r in rows]

    fig, ax = plt.subplots(figsize=(7, 4.2))
    ax.plot(xs, n0, marker="o", lw=2, color="#2057c7", label="node 0 (producer + RDMA send)")
    ax.plot(xs, n1, marker="s", lw=2, color="#8c1d40", label="node 1 (receiver + global reducer)")
    ax.plot(xs, tot, marker="^", lw=2, ls="--", color="#444", label="total wall time")
    ax.set_xticks(xs)
    ax.set_xticklabels([f"{m}MB" for m in mb])
    ax.set_xlabel("input file size")
    ax.set_ylabel("time (ms)")
    ax.set_ylim(bottom=0)
    ax.grid(True, axis="y", ls=":", alpha=0.4)
    ax.legend(frameon=False, loc="upper left")
    ax.set_title("2-node RDMA word count — node 1 (reducer) is the critical path", fontsize=12)
    fig.tight_layout()
    os.makedirs(args.outdir, exist_ok=True)
    out = os.path.join(args.outdir, f"rdma_input_scale.{args.format}")
    fig.savefig(out, dpi=150, bbox_inches="tight")
    plt.close(fig)
    print(f"[plot] wrote {out}")
    for r in rows:
        print(f"  {r['size_mb']:>4} MB: node0={r['node0_ms']}ms node1={r['node1_ms']}ms "
              f"total={r['total_ms']}ms (n1/n0={r['ratio_n1_n0']})")


if __name__ == "__main__":
    main()
