#!/usr/bin/env python3
"""Verify the COMBINED cross-node streaming test output against the baseline.

Covers, in one 2-node run:
  - streaming input (node 0 source, per-round rdma_send),
  - cross-node RDMA streaming (conn-3/4),
  - per-stage WIDTH (node 1, two middle stages width:2 — host scatter/gather),
  - Mode-2 per-round output RETURN (node 0 StreamOutput sink).

Output (written on node 0) must be byte-identical to the single-node baseline.
Run on node-0 after the run, from the project root:
    python3 Tests/Streaming_CrossNode/verify_combo.py
"""
import os, sys, zlib, glob

OUT  = os.path.join(os.path.dirname(__file__), "..", "..", "TestOutput", "combo_return_out")
BASE = os.path.join(os.path.dirname(__file__), "baseline")

def crc(p):
    with open(p, "rb") as f:
        return zlib.crc32(f.read()) & 0xffffffff

names = [os.path.basename(p) for p in sorted(glob.glob(os.path.join(BASE, "*.pgm")))]
if not names:
    print("FAIL: no baseline pgm files in", BASE); sys.exit(1)

ok = True
for n in names:
    b, o = os.path.join(BASE, n), os.path.join(OUT, n)
    if not os.path.exists(o):
        print(f"FAIL {n}: combo output missing ({o})"); ok = False; continue
    cb, co = crc(b), crc(o)
    print(("OK   " if cb == co else "FAIL ") + f"{n}: baseline={cb:08x} combo={co:08x}")
    ok = ok and cb == co

print("\nPASS — combined cross-node streaming == single-node baseline" if ok else "\nFAILED")
sys.exit(0 if ok else 1)
