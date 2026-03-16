"""
Python workloads for the WASM streaming DAG framework.

Available API (via the `shm` native module):

  shm.read_all_stream_records(slot)     -> list[ (origin: int, data: bytes) ]
  shm.read_all_inputs()                 -> list[ (origin: int, data: bytes) ]
  shm.read_all_inputs_from(io_slot)     -> list[ (origin: int, data: bytes) ]
  shm.append_stream_data(slot, data)    -> None   (data must be bytes)
  shm.write_output(data)                -> None   (bytes, written to IO slot 1)
  shm.write_output_str(s)               -> None   (str, written to IO slot 1)
"""

import shm

# ─────────────────────────────────────────────────────────────────────────────
# Word-count demo
# ─────────────────────────────────────────────────────────────────────────────

WC_DIST_BASE    = 10
WC_MAP_OUT_BASE = 100

def wc_distribute(n_workers):
    lines = shm.read_all_inputs()
    for i, (origin, line) in enumerate(lines):
        slot = WC_DIST_BASE + (i % n_workers)
        shm.append_stream_data(slot, line)


def wc_map(slot):
    counts = {}
    for origin, rec in shm.read_all_stream_records(slot):
        for token in rec.decode('utf-8', errors='replace').split():
            word = ''.join(c.lower() for c in token if c.isalpha())
            if word:
                counts[word] = counts.get(word, 0) + 1

    out_slot = slot + WC_MAP_OUT_BASE
    for word, count in counts.items():
        shm.append_stream_data(out_slot,
                               ('word=' + word + '\x1f' + str(count)).encode())


def wc_reduce(stream_slot):
    totals = {}
    for origin, rec in shm.read_all_stream_records(stream_slot):
        s = rec.decode('utf-8', errors='replace')
        if not s.startswith('word=') or '\x1f' not in s:
            continue
        body  = s[5:]
        sep   = body.index('\x1f')
        word  = body[:sep]
        count = int(body[sep + 1:])
        if word:
            totals[word] = totals.get(word, 0) + count

    shm.write_output_str('=== word_count ===')
    shm.write_output_str('unique_words=' + str(len(totals)))
    shm.write_output_str('total_occurrences=' + str(sum(totals.values())))
    for word, count in sorted(totals.items()):
        shm.write_output_str(word + ': ' + str(count))


# ─────────────────────────────────────────────────────────────────────────────
# Image pipeline demo
# ─────────────────────────────────────────────────────────────────────────────

PIPE_IO_IN = 10
PIPE_LOAD  = 20
PIPE_ROT   = 30
PIPE_GRAY  = 40
PIPE_EQ    = 50
PIPE_BLUR  = 60


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


def img_load_ppm(_arg):
    lines = shm.read_all_inputs_from(PIPE_IO_IN)
    text  = ' '.join(
        line.decode('ascii', errors='replace').strip()
        for _orig, line in lines
        if not line.lstrip().startswith(b'#')
    )
    tok = text.split()
    if not tok:
        return
    magic  = tok[0]
    w      = int(tok[1]) if len(tok) > 1 else 0
    h      = int(tok[2]) if len(tok) > 2 else 0
    maxval = int(tok[3]) if len(tok) > 3 else 255
    if w == 0 or h == 0:
        return

    ch       = 3 if magic == 'P3' else 1
    expected = w * h * ch
    pixels   = bytearray()
    for t in tok[4:4 + expected]:
        v = int(t)
        pixels.append(v if maxval in (0, 255) else (v * 255 + maxval // 2) // maxval)
    while len(pixels) < expected:
        pixels.append(0)

    shm.append_stream_data(PIPE_LOAD, _img_header(w, h, ch) + bytes(pixels))


def img_rotate(_arg):
    recs = shm.read_all_stream_records(PIPE_LOAD)
    if not recs:
        return
    decoded = _img_decode(recs[0][1])
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
    shm.append_stream_data(PIPE_ROT, _img_header(out_w, out_h, ch) + bytes(buf))


def img_grayscale(_arg):
    recs = shm.read_all_stream_records(PIPE_ROT)
    if not recs:
        return
    decoded = _img_decode(recs[0][1])
    if not decoded:
        return
    w, h, ch, pixels = decoded
    gray = bytearray(w * h)
    for i in range(w * h):
        b = i * ch
        gray[i] = (pixels[b] * 77 + pixels[b+1] * 150 + pixels[b+2] * 29) >> 8 \
                   if ch >= 3 else pixels[b]
    shm.append_stream_data(PIPE_GRAY, _img_header(w, h, 1) + bytes(gray))


def img_equalize(_arg):
    recs = shm.read_all_stream_records(PIPE_GRAY)
    if not recs:
        return
    decoded = _img_decode(recs[0][1])
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
    shm.append_stream_data(PIPE_EQ, recs[0][1][:5] + result)


def img_blur(_arg):
    recs = shm.read_all_stream_records(PIPE_EQ)
    if not recs:
        return
    decoded = _img_decode(recs[0][1])
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
    shm.append_stream_data(PIPE_BLUR, recs[0][1][:5] + bytes(buf))


def img_export_ppm(_arg):
    recs = shm.read_all_stream_records(PIPE_BLUR)
    if not recs:
        return
    decoded = _img_decode(recs[0][1])
    if not decoded:
        return
    w, h, _ch, pixels = decoded
    header = ('P5\n%d %d\n255\n' % (w, h)).encode()
    shm.write_output(header + bytes(pixels[:w * h]))


# ─────────────────────────────────────────────────────────────────────────────
# 4-stage streaming pipeline demo
# ─────────────────────────────────────────────────────────────────────────────

PIPELINE_BATCH = 20

def pipeline_source(out_slot, round_num):
    for i in range(PIPELINE_BATCH):
        v   = round_num * 1000 + i
        rec = ('r=%d,i=%02d,v=%05d' % (round_num, i, v)).encode()
        shm.append_stream_data(out_slot, rec)


def pipeline_filter(in_slot, out_slot):
    all_recs = shm.read_all_stream_records(in_slot)
    for _origin, rec in all_recs:
        s = rec.decode('utf-8', errors='replace')
        parts = s.split(',')
        if len(parts) >= 2:
            item_val = int(parts[1].split('=')[1]) if '=' in parts[1] else -1
            if item_val % 2 == 0:
                shm.append_stream_data(out_slot, rec)


def pipeline_transform(in_slot, out_slot):
    all_recs = shm.read_all_stream_records(in_slot)
    for _origin, rec in all_recs:
        shm.append_stream_data(out_slot, rec + b'|T')


def pipeline_sink(in_slot, summary_slot):
    all_recs = shm.read_all_stream_records(in_slot)
    count, value_sum = 0, 0
    for _origin, rec in all_recs:
        s = rec.decode('utf-8', errors='replace')
        parts = s.split(',')
        if len(parts) >= 3:
            v_part = parts[2].split('=')
            if len(v_part) >= 2:
                try:
                    value_sum += int(v_part[1].rstrip('|T'))
                except ValueError:
                    pass
        count += 1
    if count > 0:
        summary = ('batch_count=%d,value_sum=%d' % (count, value_sum)).encode()
        shm.append_stream_data(summary_slot, summary)
