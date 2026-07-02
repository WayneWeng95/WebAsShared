#!/usr/bin/env python3
# wasmem_mem.py — compute a WasMem run's TOTAL MEMORY COUNT from the node-agent metrics
# (node-0 aggregates every node's rss + SHM-arena usage into /tmp/node_agent_metrics.jsonl)
# and append it as a `total_mem_mb` column to the workload CSV.
#
# total_mem_mb = Σ_nodes peak(rss_bytes + shm_bump_offset) over the job window — the
# framework's real working set: anonymous RSS plus the zero-copy shared-memory arena that
# holds the RDMA-staged inputs and inter-stage data. Also reports per-node peak for context.
import json
import sys

METRICS = "/tmp/node_agent_metrics.jsonl"


def peak_by_node(since_ms):
    peak = {}
    try:
        for line in open(METRICS):
            try:
                m = json.loads(line)
            except Exception:
                continue
            if m.get("timestamp_ms", 0) < since_ms:
                continue
            nid = m.get("node_id")
            if nid is None:
                continue
            v = m.get("rss_bytes", 0) + m.get("shm_bump_offset", 0)
            if v > peak.get(nid, 0):
                peak[nid] = v
    except FileNotFoundError:
        pass
    return peak


def main():
    csv_path, since = sys.argv[1], int(sys.argv[2])
    peak = peak_by_node(since)
    total_mb = sum(peak.values()) / 1e6
    max_mb = (max(peak.values()) / 1e6) if peak else 0.0
    # append the column to the CSV header + the last (just-written) data row
    try:
        lines = open(csv_path).read().splitlines()
    except FileNotFoundError:
        print("total_mem_mb=%.0f (no csv)" % total_mb)
        return
    if lines and "total_mem_mb" not in lines[0]:
        lines[0] = lines[0] + ",total_mem_mb,mem_max_mb"
        lines[-1] = lines[-1] + (",%.0f,%.0f" % (total_mb, max_mb))
        open(csv_path, "w").write("\n".join(lines) + "\n")
    print("total_mem_mb=%.0f mem_max_mb=%.0f (nodes=%d)" % (total_mb, max_mb, len(peak)))


if __name__ == "__main__":
    main()
