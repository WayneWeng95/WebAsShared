#!/usr/bin/env python3
"""Compare Mode-2 (per-round output RETURN) sink output to the single-node baseline.

In Mode 2 the processed images are returned over RDMA and written ON NODE 0
(the coordinator), one file per round, by the StreamOutput sink.

Run on NODE-0 after the run, from the WebAsShared project root:
    python3 Tests/Streaming_CrossNode/verify_mode2.py

Exit 0 = byte-identical to baseline (per-round return is correct),
exit 1 = mismatch / missing files.
"""
import os, sys, zlib, glob

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
OUT = os.path.join(ROOT, "TestOutput", "mode2_return_out")
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
        print(f"FAIL {n}: mode-2 return output missing ({o})"); ok = False; continue
    cb, co = crc(b), crc(o)
    tag = "OK  " if cb == co else "FAIL"
    if cb != co: ok = False
    print(f"{tag} {n}: baseline={cb:08x} mode2={co:08x}")

print("\nPASS — mode-2 per-round return == single-node baseline" if ok else "\nFAILED")
sys.exit(0 if ok else 1)
