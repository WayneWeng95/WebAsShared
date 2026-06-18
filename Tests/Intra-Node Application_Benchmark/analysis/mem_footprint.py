#!/usr/bin/env python3
"""mem_footprint.py — combined peak-memory table per workload and load.

For every (workload, load-config) it reports peak memory *including external KV
storage* for all frameworks:

    WasMem-JIT, WasMem-AOT, RMMap, Faasm, Cloudburst

Footprint definition (per run):
    total_mb = peak_mem_mb + kv_storage_mb
  - WasMem (JIT/AOT): peak_mem_mb only; shared-memory substrate, no external KV.
  - Baselines: process/billable peak_mem_mb + resident KV bytes written
    (kvs_ser_mb, or kvs_put_mb where that is the column name). `kvs_get_mb`
    (read traffic) is NOT added, to avoid double-counting stored bytes.

Outputs markdown tables (stdout + mem_footprint.md) and a tidy combined CSV
(mem_footprint.csv).
"""
import csv
import os

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# workload -> (load-key columns, human label for size)
WORKLOADS = [
    ("WordCount",    ["size_mb", "workers"],     "size_mb"),
    ("Finra",        ["size_trades"],            "size_trades"),
    ("Matrix",       ["size_n", "workers"],      "size_n"),
    ("ML_training",  ["size_mb", "workers"],     "size_mb"),
    ("ML_inference", ["size_mb", "workers"],     "size_mb"),
]

FRAMEWORKS = ["WasMem-JIT", "WasMem-AOT", "RMMap", "Faasm", "Cloudburst"]

# relative path of each framework's results.csv inside a workload dir
PATHS = {
    "WasMem-JIT":  "results.csv",
    "WasMem-AOT":  "results_aot.csv",
    "RMMap":       os.path.join("baseline", "rmmap", "results.csv"),
    "Faasm":       os.path.join("baseline", "faasm", "demo", "results.csv"),
    "Cloudburst":  os.path.join("baseline", "cloudburst", "results.csv"),
}


def fnum(x):
    try:
        return float(x)
    except (TypeError, ValueError):
        return None


def load(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return list(csv.DictReader(f))


def kv_storage(row):
    """Resident KV bytes written for a baseline row (MB)."""
    for col in ("kvs_ser_mb", "kvs_put_mb", "state_kv_mb"):
        if col in row and fnum(row[col]) is not None:
            return fnum(row[col])
    return 0.0


def footprint(framework, row):
    """Return total peak memory incl. KV (MB), or None for CRASH/missing.

    Faasm's driver now records the CONCURRENT footprint directly (Σ RSS of the
    co-resident Faaslets during the map phase — see baseline/faasm/demo/driver.py),
    so no post-hoc fan-out correction is applied here; it is treated like any
    other baseline (`peak_mem_mb + KV storage`).
    """
    peak = fnum(row.get("peak_mem_mb"))
    if peak is None:
        return None
    if framework.startswith("WasMem"):
        return peak            # SHM already in peak; no external KV
    return peak + kv_storage(row)


def load_key(row, keycols):
    return tuple(row[c] for c in keycols)


def fmt(v):
    if v is None:
        return "CRASH"
    return f"{v:,.1f}"


def build():
    md = []
    combined = []  # tidy rows for csv
    for wl, keycols, _ in WORKLOADS:
        wdir = os.path.join(ROOT, wl)
        # framework -> {load_key: row}
        data = {}
        order = []  # preserve load order from JIT file (fullest sweep)
        seen = set()
        for fw in FRAMEWORKS:
            rows = load(os.path.join(wdir, PATHS[fw]))
            data[fw] = {load_key(r, keycols): r for r in rows}
            for r in rows:
                k = load_key(r, keycols)
                if k not in seen:
                    seen.add(k)
                    order.append(k)

        header = keycols + [f"{fw} (MB)" for fw in FRAMEWORKS]
        md.append(f"\n### {wl}\n")
        md.append("| " + " | ".join(header) + " |")
        md.append("|" + "|".join(["---"] * len(header)) + "|")
        for k in order:
            cells = list(k)
            rec = {kc: kv for kc, kv in zip(keycols, k)}
            for fw in FRAMEWORKS:
                row = data[fw].get(k)
                val = footprint(fw, row) if row else None
                cells.append(fmt(val))
                rec[fw] = "" if val is None else round(val, 1)
            md.append("| " + " | ".join(cells) + " |")
            rec["workload"] = wl
            combined.append(rec)
    return "\n".join(md), combined


COMMON_NOTES = (
    "All values are MB. `total = peak_mem_mb + KV-storage`; WasMem has no\n"
    "external KV (shared-memory substrate). Baseline KV storage = resident\n"
    "bytes written (`kvs_ser_mb`/`kvs_put_mb`/`state_kv_mb`); read traffic\n"
    "(`kvs_get_mb`) is excluded. CRASH = OOM in our sweep.\n"
    "\n**Faasm** `peak_mem_mb`: **WordCount** is the high-water Σ RSS of the whole\n"
    "driver process tree (driver + co-resident Faaslets), the same whole-tree\n"
    "accounting WasMem applies to its host tree — measured the same way on both\n"
    "sides. The remaining workloads still report the older per-Faaslet concurrent\n"
    "footprint (Σ private RSS + shared-once) pending the same migration.\n"
)


def write_tables(md_name, csv_name):
    md, combined = build()
    title = (
        "# Peak Memory Footprint (incl. external KV storage) — per workload & load\n\n"
        + COMMON_NOTES
    )
    out = title + md + "\n"
    with open(os.path.join(HERE, md_name), "w") as f:
        f.write(out)

    cols = ["workload", "size_mb", "size_trades", "size_n", "workers"] + FRAMEWORKS
    with open(os.path.join(HERE, csv_name), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols, extrasaction="ignore")
        w.writeheader()
        for rec in combined:
            w.writerow(rec)
    return out


def main():
    out = write_tables("mem_footprint.md", "mem_footprint.csv")
    print(out)
    print("\n[wrote] mem_footprint.md / .csv")


if __name__ == "__main__":
    main()
