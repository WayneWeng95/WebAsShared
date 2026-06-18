#!/usr/bin/env python3
"""gen_records.py — deterministic Gensort-style 100-byte records for TeraSort.

Each record is ONE line of exactly 100 bytes:
    bytes 0..10   : 10-byte sort key, uniform random over printable ASCII [33..126]
    bytes 10..99  : 89-byte payload (constant filler — irrelevant to the sort)
    byte  99      : '\\n'

The host loads the file line-by-line (newline stripped), so each guest record is
the 99-byte body and the key is its first 10 bytes. Keys are uniform over the
key space, so fixed equal-width range splitters (owner = first_byte·N/SPAN in the
guest) are balanced — no sampling stage needed.

Deterministic: seeded RNG → identical file (and therefore identical sort
checksum) across systems and reruns. All four systems sort the SAME input.

Usage:
    gen_records.py <size_mb> <out_path> [seed]
"""
import os
import random
import sys

REC_LEN = 100          # bytes per record incl. newline (Gensort convention)
KEY_LEN = 10
PAYLOAD = b"." * (REC_LEN - KEY_LEN - 1)   # 89-byte filler
LO, HI = 33, 96        # printable-ASCII key-space bounds (must match guest)
SPAN = HI - LO + 1     # 64 symbols (256 % 64 == 0 → bias-free uniform keys)

# Map a random byte to a key symbol in [LO, HI] via a 256-entry translate table.
_TABLE = bytes(LO + (i % SPAN) for i in range(256))

BATCH = 200_000        # records assembled/written per batch


def main():
    if len(sys.argv) < 3:
        sys.exit("usage: gen_records.py <size_mb> <out_path> [seed]")
    size_mb = float(sys.argv[1])
    out_path = sys.argv[2]
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1234

    n_records = int(round(size_mb * 1024 * 1024 / REC_LEN))
    rng = random.Random(seed)
    os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)

    written = 0
    with open(out_path, "wb") as f:
        while written < n_records:
            batch = min(BATCH, n_records - written)
            raw = rng.randbytes(batch * KEY_LEN).translate(_TABLE)
            lines = [raw[k * KEY_LEN:(k + 1) * KEY_LEN] + PAYLOAD + b"\n"
                     for k in range(batch)]
            f.write(b"".join(lines))
            written += batch

    print("wrote %s  (%d records, %.1f MB)"
          % (out_path, n_records, n_records * REC_LEN / 1048576))


if __name__ == "__main__":
    main()
