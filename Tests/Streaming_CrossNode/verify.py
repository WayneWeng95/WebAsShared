#!/usr/bin/env python3
"""Compare cross-node streaming output (node-1) to the single-node baseline.

Run on NODE-1 after the run, from the WebAsShared project root:
    python3 Tests/Streaming_CrossNode/verify.py

Exit 0 = byte-identical to baseline (cross-node pipelining is correct),
exit 1 = mismatch / missing files.
"""
import os, sys, zlib, glob

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
OUT = os.path.join(ROOT, "TestOutput", "rdma_img_pipeline_out")
BASE = os.path.join(os.path.dirname(__file__), "baseline")

def crc(p):
    with open(p, "rb") as f:
        return zlib.crc32(f.read()) & 0xffffffff

names = [os.path.basename(p) for p in sorted(glob.glob(os.path.join(BASE, "*.pgm")))]
if not names:
    print("FAIL: no baseline pgm files staged in", BASE); sys.exit(1)

ok = True
for n in names:
    b = os.path.join(BASE, n); o = os.path.join(OUT, n)
    if not os.path.exists(o):
        print(f"FAIL {n}: cross-node output missing ({o})"); ok = False; continue
    cb, co = crc(b), crc(o)
    tag = "OK  " if cb == co else "FAIL"
    if cb != co: ok = False
    print(f"{tag} {n}: baseline={cb:08x} crossnode={co:08x}")

print("\nPASS — cross-node == single-node baseline" if ok else "\nFAILED")
sys.exit(0 if ok else 1)
