"""Image processing workloads — PPM load, rotate, grayscale, equalize, blur, export.

All functions take explicit (in_slot, out_slot) arguments supplied by the DAG
JSON — no slot numbers are hardcoded here.

Two load variants exist because the input record format differs:
  img_load_ppm        — grouping mode: many text-line records joined then parsed
  img_load_ppm_binary — pipeline mode: one binary PPM record per round (cursor-based)
"""

import shm


# ── Internal helpers ──────────────────────────────────────────────────────────

def _img_decode(data):
    """Return (w, h, ch, pixels_bytes) or None."""
    if len(data) < 5:
        return None
    w  = data[0] | (data[1] << 8)
    h  = data[2] | (data[3] << 8)
    ch = data[4]
    if len(data) < 5 + w * h * ch:
        return None
    return w, h, ch, data[5:]


def _img_header(w, h, ch):
    return bytes([w & 0xFF, (w >> 8) & 0xFF,
                  h & 0xFF, (h >> 8) & 0xFF, ch])


def _img_parse_ppm_text(text):
    """Parse a PPM text (P3/P5 ASCII) into (w, h, ch, pixels_bytearray) or None."""
    tok = [t for t in text.split() if not t.startswith('#')]
    if len(tok) < 4:
        return None
    magic  = tok[0]
    w      = int(tok[1])
    h      = int(tok[2])
    maxval = int(tok[3])
    if w == 0 or h == 0:
        return None
    ch       = 3 if magic == 'P3' else 1
    expected = w * h * ch
    pixels   = bytearray()
    for t in tok[4:4 + expected]:
        v = int(t)
        pixels.append(v if maxval in (0, 255) else (v * 255 + maxval // 2) // maxval)
    while len(pixels) < expected:
        pixels.append(0)
    return w, h, ch, pixels


def _read_stream_latest(in_slot):
    """Return the payload of the most recently appended record in a stream slot, or None."""
    recs = shm.read_all_stream_records(in_slot)
    return recs[-1][1] if recs else None


# ── Public workload functions ─────────────────────────────────────────────────

def img_load_ppm(in_slot, out_slot):
    """Grouping mode: join all text-line records from IO slot `in_slot`, parse as
    PPM text (P3/P5), write decoded pixel blob to stream slot `out_slot`.

    Used by PyGrouping where the host Input node loads the PPM file as text lines,
    one record per line.
    """
    if shm.count_io_records(in_slot) == 0:
        return
    lines = shm.read_all_inputs_from(in_slot)
    text  = ' '.join(
        line.decode('ascii', errors='replace').strip()
        for _orig, line in lines
        if not line.lstrip().startswith(b'#')
    )
    parsed = _img_parse_ppm_text(text)
    if parsed is None:
        return
    w, h, ch, pixels = parsed
    shm.append_stream_data(out_slot, _img_header(w, h, ch) + bytes(pixels))


def img_load_ppm_binary(in_slot, out_slot):
    """Pipeline mode: read the next binary PPM record from IO slot `in_slot`
    (one full PPM file per record, cursor-based), parse, write decoded pixel
    blob to stream slot `out_slot`.

    Used by PyPipeline where the host Input node loads PPM files in binary mode,
    one record per image.
    """
    rec = shm.read_next_io_record(in_slot)
    if rec is None:
        return
    parsed = _img_parse_ppm_text(rec[1].decode('ascii', errors='replace'))
    if parsed is None:
        return
    w, h, ch, pixels = parsed
    shm.append_stream_data(out_slot, _img_header(w, h, ch) + bytes(pixels))


def img_rotate(in_slot, out_slot):
    """Rotate image 90° clockwise.  Reads from stream `in_slot`, writes to stream `out_slot`."""
    raw = _read_stream_latest(in_slot)
    if raw is None:
        return
    decoded = _img_decode(raw)
    if not decoded:
        return
    w, h, ch, pixels = decoded
    out_w, out_h = h, w
    buf = bytearray(out_w * out_h * ch)
    for r in range(out_h):
        for c in range(out_w):
            src = ((h - 1 - c) * w + r) * ch
            dst = (r * out_w + c) * ch
            buf[dst:dst + ch] = pixels[src:src + ch]
    shm.append_stream_data(out_slot, _img_header(out_w, out_h, ch) + bytes(buf))


def img_grayscale(in_slot, out_slot):
    """Convert image to grayscale.  Reads from stream `in_slot`, writes to stream `out_slot`."""
    raw = _read_stream_latest(in_slot)
    if raw is None:
        return
    decoded = _img_decode(raw)
    if not decoded:
        return
    w, h, ch, pixels = decoded
    gray = bytearray(w * h)
    for i in range(w * h):
        b = i * ch
        gray[i] = (pixels[b] * 77 + pixels[b+1] * 150 + pixels[b+2] * 29) >> 8 \
                   if ch >= 3 else pixels[b]
    shm.append_stream_data(out_slot, _img_header(w, h, 1) + bytes(gray))


def img_equalize(in_slot, out_slot):
    """Histogram-equalize a grayscale image.  Reads from stream `in_slot`, writes to stream `out_slot`."""
    raw = _read_stream_latest(in_slot)
    if raw is None:
        return
    decoded = _img_decode(raw)
    if not decoded:
        return
    w, h, _ch, pixels = decoded
    total = w * h
    hist  = [0] * 256
    for p in pixels[:total]:
        hist[p] += 1
    cdf     = []
    running = 0
    for v in hist:
        running += v
        cdf.append(running)
    cdf_min = next((x for x in cdf if x > 0), 0)
    denom   = max(total - cdf_min, 1)
    lut     = [(cdf[i] - cdf_min) * 255 // denom for i in range(256)]
    result  = bytes([lut[p] for p in pixels[:total]])
    shm.append_stream_data(out_slot, raw[:5] + result)


def img_blur(in_slot, out_slot):
    """Apply a 3×3 box blur.  Reads from stream `in_slot`, writes to stream `out_slot`."""
    raw = _read_stream_latest(in_slot)
    if raw is None:
        return
    decoded = _img_decode(raw)
    if not decoded:
        return
    w, h, _ch, pixels = decoded
    buf = bytearray(w * h)
    for r in range(h):
        for c in range(w):
            s, cnt = 0, 0
            for nr in range(max(0, r - 1), min(h, r + 2)):
                for nc in range(max(0, c - 1), min(w, c + 2)):
                    s   += pixels[nr * w + nc]
                    cnt += 1
            buf[r * w + c] = s // cnt
    shm.append_stream_data(out_slot, raw[:5] + bytes(buf))


def img_export_ppm(in_slot, out_slot):
    """Encode a pixel blob as a PGM file and write it to I/O slot `out_slot`.
    Reads from stream `in_slot`.
    """
    raw = _read_stream_latest(in_slot)
    if raw is None:
        return
    decoded = _img_decode(raw)
    if not decoded:
        return
    w, h, _ch, pixels = decoded
    pgm = ('P5\n%d %d\n255\n' % (w, h)).encode() + bytes(pixels[:w * h])
    shm.write_io(out_slot, pgm)
