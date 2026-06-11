#!/usr/bin/env python3
"""Locality-hint structural test (LOCAL — no cluster needed).

Partitions locality_demo.json in three hint orientations and asserts the
cross-machine cut (the injected RemoteSend/RemoteRecv pair) lands on the edge the
`split` hint marks as breakable:

  - heavy=avoid, light=prefer  → cut on the LIGHT edge (heavy kept local)
  - heavy=prefer, light=avoid  → cut FLIPS to the HEAVY edge
  - no hints (neutral)         → baseline cut (documents default placement)

Run from the project root:  python3 Tests/Cluster_Eval/check_locality.py
Exit 0 = the hint deterministically controls where the topology is broken.
"""
import os, sys, json, copy, tempfile, subprocess

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
PART = os.path.join(ROOT, "Partitioner", "target", "release", "partition")
DEMO = os.path.join(os.path.dirname(__file__), "locality_demo.json")

ok = True
def check(name, cond, detail=""):
    global ok
    print(("  PASS  " if cond else "  FAIL  ") + name + (("  — " + detail) if (detail and not cond) else ""))
    ok = ok and cond


def set_split(dag, node_id, value):
    """Return a copy of `dag` with node `node_id`'s split hint set/removed."""
    d = copy.deepcopy(dag)
    for n in d["nodes"]:
        if n["id"] == node_id:
            if value is None:
                n.pop("split", None)
            else:
                n["split"] = value
    return d


def cut_source(dag):
    """Partition `dag` (2 nodes, empty hints) and return the source id of the
    cross-machine cut, i.e. the producer whose edge was broken (from the injected
    `rs_<src>_to_<dst>` node)."""
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        json.dump(dag, f); dpath = f.name
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        f.write("{}"); hpath = f.name   # empty hints → every host has quota
    try:
        res = subprocess.run([PART, dpath, "--nodes", "2", "--hints", hpath],
                             capture_output=True, text=True, cwd=ROOT)
        if res.returncode != 0:
            raise SystemExit(f"partition failed: {res.stderr.strip()}")
        cluster = json.loads(res.stdout)
    finally:
        os.remove(dpath); os.remove(hpath)
    for nodes in cluster["node_dags"].values():
        for n in nodes:
            if n["id"].startswith("rs_"):           # rs_<src>_to_<dst>
                return n["id"][len("rs_"):].rsplit("_to_", 1)[0]
    return None


def main():
    if not os.path.exists(PART):
        raise SystemExit("partition binary missing — run ./build.sh first")
    base = json.load(open(DEMO))
    print("locality-hint structural test (partition-only, no cluster)\n")

    # As authored: heavy=avoid, light=prefer.
    src = cut_source(base)
    check(f"hinted (heavy=avoid, light=prefer): cut on LIGHT edge (got '{src}')", src == "light")

    # Flip the hints: heavy=prefer, light=avoid → cut must move to the heavy edge.
    rev = set_split(set_split(base, "heavy", "prefer"), "light", "avoid")
    src_rev = cut_source(rev)
    check(f"reversed (heavy=prefer, light=avoid): cut FLIPS to HEAVY edge (got '{src_rev}')", src_rev == "heavy")

    # Sanity: the hint actually changed the decision.
    check("the split hint changed which edge is cut", src != src_rev,
          f"both orientations cut '{src}' — hint had no effect")

    # Neutral (documentational): no hints → baseline placement.
    neu = set_split(set_split(base, "heavy", None), "light", None)
    print(f"  (neutral baseline, no hints: cut on '{cut_source(neu)}' edge)")

    print("\n=== " + ("PASS — split hint controls the cut" if ok else "FAILED") + " ===")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
