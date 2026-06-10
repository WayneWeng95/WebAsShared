#!/usr/bin/env python3
"""
Generate sized image payloads for the image-resize / fanout benchmark, in OUR
internal binary image record format so the guest functions consume them with no
PPM parsing:

    [w: u16 LE][h: u16 LE][ch: u8=3][pixels: w*h*3 bytes]   (5-byte header)

This matches `img_decode` in Executor/guest/src/workloads/img_pipeline.rs. The
same bytes are the fanned-out payload for img_noop / img_resize, and are sized to
match Roadrunner's text payloads (1/10/50/100 MB) so delivery cost is comparable.

w and h are u16, so each dimension must stay < 65536 — fine up to multi-GB images.
Pixels are a deterministic tiled gradient (reproducible, not all-zeros).

Usage:
    python3 Tests/ImageResize_Fanout/gen_images.py            # 1 10 50 100 MB
    SIZES="1 10 50" python3 Tests/ImageResize_Fanout/gen_images.py
    python3 Tests/ImageResize_Fanout/gen_images.py 1 10 50

Writes to TestData/fanout_img/<N>MB.img (under the project root).
"""
import os, sys, math, pathlib, struct

ROOT = pathlib.Path(__file__).resolve().parents[2]   # WebAsShared/
OUT = ROOT / "TestData" / "fanout_img"
CH = 3

def sizes_from_args():
    if len(sys.argv) > 1:
        return [int(x) for x in sys.argv[1:]]
    env = os.environ.get("SIZES")
    if env:
        return [int(x) for x in env.split()]
    return [1, 10, 50, 100]

def gen_one(mb: int, path: pathlib.Path):
    target = mb * 1024 * 1024
    px = max(1, (target - 5) // CH)        # solve w*h*CH ≈ target, square-ish
    w = int(math.isqrt(px))
    h = px // w
    if w >= 65536 or h >= 65536:
        raise ValueError(f"{mb}MB needs dim >= 65536 (w={w},h={h}); u16 header can't hold it")
    header = struct.pack("<HHB", w, h, CH)
    row = bytes((i * 37 + 11) & 0xFF for i in range(w * CH))   # tiled gradient
    with open(path, "wb") as f:
        f.write(header)
        for _ in range(h):
            f.write(row)
    actual = len(header) + w * h * CH
    print(f"wrote {path}  ({w}x{h}x{CH}, {actual/1024/1024:.2f} MB)")

def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for mb in sizes_from_args():
        gen_one(mb, OUT / f"{mb}MB.img")

if __name__ == "__main__":
    main()
