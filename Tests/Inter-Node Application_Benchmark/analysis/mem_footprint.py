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


def footprint(framework, row, faasm_concurrent=True):
    """Return total peak memory incl. KV (MB), or None for CRASH/missing.

    Faasm correction (faasm_concurrent=True): the driver records the MAX single-
    Faaslet RSS, but at fan-out N the map phase keeps N wasmtime instances
    resident at once. Since per-Faaslet RSS is ~constant across the N mappers,
    the true concurrent footprint is N x peak_mem_mb. Applied only where a
    `workers` column exists (WordCount/Matrix/ML_*); Finra has no fan-out column
    so it is left as-is. With faasm_concurrent=False the raw recorded RSS is used.
    """
    peak = fnum(row.get("peak_mem_mb"))
    if peak is None:
        return None
    if framework.startswith("WasMem"):
        return peak            # SHM already in peak; no external KV
    if framework == "Faasm" and faasm_concurrent:
        n = fnum(row.get("workers")) or 1.0
        return n * peak + kv_storage(row)
    return peak + kv_storage(row)


def load_key(row, keycols):
    return tuple(row[c] for c in keycols)


def fmt(v):
    if v is None:
        return "CRASH"
    return f"{v:,.1f}"


def build(faasm_concurrent=True):
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
                val = footprint(fw, row, faasm_concurrent) if row else None
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
)

FAASM_CONCURRENT_NOTE = (
    "\n**Faasm = concurrent estimate**: `workers x per-Faaslet RSS + KV`. The\n"
    "driver records the max single-Faaslet RSS, but N Faaslets are resident\n"
    "together during the map phase, so the concurrent footprint is N x that\n"
    "RSS. Finra has no fan-out column and is left at the recorded RSS.\n"
)

FAASM_RAW_NOTE = (
    "\n**Faasm = raw recorded RSS** (as emitted by the driver): the max single-\n"
    "Faaslet RSS + KV, NOT multiplied by fan-out. This undercounts the true\n"
    "concurrent footprint at high N (N Faaslets coexist during the map phase);\n"
    "see `mem_footprint.md` for the N x concurrent estimate.\n"
)


def write_variant(faasm_concurrent, md_name, csv_name, faasm_note):
    md, combined = build(faasm_concurrent)
    title = (
        "# Peak Memory Footprint (incl. external KV storage) — per workload & load\n\n"
        + COMMON_NOTES + faasm_note
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
    # corrected (Faasm N x concurrent) + raw (Faasm single-instance, the old view)
    out = write_variant(True, "mem_footprint.md", "mem_footprint.csv",
                         FAASM_CONCURRENT_NOTE)
    write_variant(False, "mem_footprint_raw.md", "mem_footprint_raw.csv",
                  FAASM_RAW_NOTE)
    print(out)
    print("\n[wrote] mem_footprint.md / .csv (Faasm concurrent estimate)")
    print("[wrote] mem_footprint_raw.md / .csv (Faasm raw single-instance RSS)")


if __name__ == "__main__":
    main()
