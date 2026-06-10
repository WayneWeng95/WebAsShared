// ─────────────────────────────────────────────────────────────────────────────
// Image processing pipeline demo — slot arguments driven by the DAG JSON
//
// Every function follows the StreamPipeline two-arg convention:
//   (in_slot, out_slot)  —  read from in_slot, write to out_slot
//
// Boundary stages:
//   img_load_ppm  (io_slot, out_stream)  — I/O area → stream area
//   img_export_ppm(in_stream, out_io)    — stream area → I/O area
//
// Intermediate stages (stream → stream):
//   img_rotate, img_grayscale, img_equalize, img_blur
//
// All stages use cursor-based reads (read_next_*) so each round processes
// exactly one image and returns early (no-op) when no new record has arrived.
// Cursors are stored as named SHM atomics keyed by slot, surviving across
// the subprocess boundary between pipeline ticks.
//
// Internal image record format (one SHM record per image):
//   [width: u16 LE][height: u16 LE][channels: u8][pixels: w*h*ch bytes]
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

/// Decode a 5-byte image header. Returns (width, height, channels, pixel_slice).
#[inline]
fn img_decode(data: &[u8]) -> Option<(usize, usize, usize, &[u8])> {
    if data.len() < 5 { return None; }
    let w  = u16::from_le_bytes([data[0], data[1]]) as usize;
    let h  = u16::from_le_bytes([data[2], data[3]]) as usize;
    let ch = data[4] as usize;
    if data.len() < 5 + w * h * ch { return None; }
    Some((w, h, ch, &data[5..]))
}

#[inline]
fn img_header(w: usize, h: usize, ch: usize) -> [u8; 5] {
    let wb = (w as u16).to_le_bytes();
    let hb = (h as u16).to_le_bytes();
    [wb[0], wb[1], hb[0], hb[1], ch as u8]
}

/// Load the next unread ASCII PPM (P3) or PGM (P2) image from I/O slot `io_slot`
/// and write the internal binary image format to stream slot `out_slot`.
/// Returns immediately (no-op) if no new record is available.
#[no_mangle]
pub extern "C" fn img_load_ppm(io_slot: u32, out_slot: u32) {
    let (_origin, raw) = match ShmApi::read_next_io_record(io_slot) {
        Some(r) => r,
        None => return,
    };

    let mut all = alloc::string::String::new();
    for line in raw.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        if let Ok(s) = core::str::from_utf8(line) {
            if !s.trim_start().starts_with('#') {
                all.push_str(s.trim());
                all.push(' ');
            }
        }
    }

    let mut tok = all.split_whitespace();
    let magic       = tok.next().unwrap_or("");
    let w: usize    = tok.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let h: usize    = tok.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let maxval: u32 = tok.next().and_then(|s| s.parse().ok()).unwrap_or(255);
    if w == 0 || h == 0 { return; }

    let ch = if magic == "P3" { 3usize } else { 1 };
    let expected = w * h * ch;
    let mut pixels = Vec::with_capacity(expected);
    for t in tok.take(expected) {
        if let Ok(v) = t.parse::<u32>() {
            pixels.push(if maxval == 0 || maxval == 255 { v as u8 }
                        else { ((v * 255 + maxval / 2) / maxval) as u8 });
        }
    }
    pixels.resize(expected, 0);

    let mut data = Vec::with_capacity(5 + expected);
    data.extend_from_slice(&img_header(w, h, ch));
    data.extend_from_slice(&pixels);
    ShmApi::append_stream_data(out_slot, &data);
}

/// Rotate 90° CW: reads next record from stream `in_slot`, writes to stream `out_slot`.
#[no_mangle]
pub extern "C" fn img_rotate(in_slot: u32, out_slot: u32) {
    let (_origin, data) = match ShmApi::read_next_stream_record(in_slot) { Some(r) => r, None => return };
    let (w, h, ch, pixels) = match img_decode(&data) { Some(v) => v, None => return };
    let (out_w, out_h) = (h, w);
    let mut buf = vec![0u8; out_w * out_h * ch];
    for r in 0..out_h {
        for c in 0..out_w {
            let src = ((h - 1 - c) * w + r) * ch;
            let dst = (r * out_w + c) * ch;
            buf[dst..dst + ch].copy_from_slice(&pixels[src..src + ch]);
        }
    }
    let mut out = Vec::with_capacity(5 + buf.len());
    out.extend_from_slice(&img_header(out_w, out_h, ch));
    out.extend_from_slice(&buf);
    ShmApi::append_stream_data(out_slot, &out);
}

/// Grayscale: reads next record from stream `in_slot`, writes to stream `out_slot`.
#[no_mangle]
pub extern "C" fn img_grayscale(in_slot: u32, out_slot: u32) {
    let (_origin, data) = match ShmApi::read_next_stream_record(in_slot) { Some(r) => r, None => return };
    let (w, h, ch, pixels) = match img_decode(&data) { Some(v) => v, None => return };
    let mut out = Vec::with_capacity(5 + w * h);
    out.extend_from_slice(&img_header(w, h, 1));
    for i in 0..w * h {
        let b = i * ch;
        let gray = if ch >= 3 {
            ((pixels[b] as u32 * 77 + pixels[b+1] as u32 * 150 + pixels[b+2] as u32 * 29) >> 8) as u8
        } else { pixels[b] };
        out.push(gray);
    }
    ShmApi::append_stream_data(out_slot, &out);
}

/// Histogram equalisation: reads next record from stream `in_slot`, writes to stream `out_slot`.
#[no_mangle]
pub extern "C" fn img_equalize(in_slot: u32, out_slot: u32) {
    let (_origin, data) = match ShmApi::read_next_stream_record(in_slot) { Some(r) => r, None => return };
    let (w, h, _ch, pixels) = match img_decode(&data) { Some(v) => v, None => return };
    let total = w * h;
    let mut hist = [0u32; 256];
    for &p in &pixels[..total] { hist[p as usize] += 1; }
    let mut cdf = [0u32; 256];
    let mut running = 0u32;
    for i in 0..256 { running += hist[i]; cdf[i] = running; }
    let cdf_min = cdf.iter().copied().find(|&x| x > 0).unwrap_or(0);
    let denom = (total as u32).saturating_sub(cdf_min);
    let mut lut = [0u8; 256];
    for i in 0..256 {
        lut[i] = if denom == 0 { i as u8 } else {
            let n = cdf[i].saturating_sub(cdf_min);
            ((n as u64 * 255 + denom as u64 / 2) / denom as u64) as u8
        };
    }
    let mut out = Vec::with_capacity(5 + total);
    out.extend_from_slice(&data[..5]);
    for &p in &pixels[..total] { out.push(lut[p as usize]); }
    ShmApi::append_stream_data(out_slot, &out);
}

/// 3×3 box blur: reads next record from stream `in_slot`, writes to stream `out_slot`.
#[no_mangle]
pub extern "C" fn img_blur(in_slot: u32, out_slot: u32) {
    let (_origin, data) = match ShmApi::read_next_stream_record(in_slot) { Some(r) => r, None => return };
    let (w, h, _ch, pixels) = match img_decode(&data) { Some(v) => v, None => return };
    let mut out = Vec::with_capacity(5 + w * h);
    out.extend_from_slice(&data[..5]);
    for r in 0..h {
        for c in 0..w {
            let mut sum = 0u32; let mut cnt = 0u32;
            for nr in r.saturating_sub(1)..=(r + 1).min(h - 1) {
                for nc in c.saturating_sub(1)..=(c + 1).min(w - 1) {
                    sum += pixels[nr * w + nc] as u32; cnt += 1;
                }
            }
            out.push((sum / cnt) as u8);
        }
    }
    ShmApi::append_stream_data(out_slot, &out);
}

// ─────────────────────────────────────────────────────────────────────────────
// Fanout / data-delivery workload (Roadrunner comparison — Benchmarks #1)
//
// Mirrors Roadrunner's one-to-many "image-resizing data delivery": one image is
// fanned out to N worker functions over our zero-copy page-chain (intra-node) or
// RDMA (inter-node) substrate. The host `Shuffle{Broadcast}` splices the image
// page chain into N downstream slots WITHOUT copying the bytes; each worker then
// runs one of:
//   img_noop   — pure transport (receive + return), the apples-to-apples with
//                Roadrunner's receive-and-return func_b. Headline number.
//   img_resize — transport + a single resize step (downscale 0.5, nearest).
//
// Input image format is our internal binary record (see top of file):
//   [w: u16 LE][h: u16 LE][ch: u8][pixels: w*h*ch]
// generated by Tests/ImageResize_Fanout/gen_images.py. No PPM parsing needed —
// img_ingest just moves the bytes from the I/O area into a stream slot so the
// host broadcast can splice them.
// ─────────────────────────────────────────────────────────────────────────────

/// Staging: move the next binary image record from I/O slot `io_slot` into
/// stream slot `out_slot` verbatim (no parsing). Run as a 1-round StreamPipeline
/// stage before the broadcast. Staging is excluded from the measured fanout wave.
#[no_mangle]
pub extern "C" fn img_ingest(io_slot: u32, out_slot: u32) {
    let (_origin, raw) = match ShmApi::read_next_io_record(io_slot) {
        Some(r) => r,
        None => return,
    };
    ShmApi::append_stream_data(out_slot, &raw);
}

/// Worker (pure transport): receive the delivered image from stream slot `slot`
/// and return. Reads the record once (so the delivery materialises, exactly like
/// Roadrunner's func_b receiving the bytes) and does no compute.
#[no_mangle]
pub extern "C" fn img_noop(slot: u32) {
    // `read_next` advances the per-slot cursor; touch one byte through a
    // volatile read so the receive can't be optimised away.
    if let Some((_origin, data)) = ShmApi::read_next_stream_record(slot) {
        if !data.is_empty() {
            unsafe { core::ptr::read_volatile(&data[0]); }
        }
    }
}

/// Worker (transport + compute): receive the delivered image and resize it by a
/// fixed 0.5 factor (nearest-neighbour). The resized buffer is computed and then
/// dropped — the benchmark measures delivery + resize cost, not output storage,
/// so no output slot is needed (keeps the single-arg WasmVoid signature).
#[no_mangle]
pub extern "C" fn img_resize(slot: u32) {
    let (_origin, data) = match ShmApi::read_next_stream_record(slot) { Some(r) => r, None => return };
    let (w, h, ch, pixels) = match img_decode(&data) { Some(v) => v, None => return };
    let (out_w, out_h) = (w / 2, h / 2);
    if out_w == 0 || out_h == 0 { return; }
    let mut buf = vec![0u8; out_w * out_h * ch];
    for r in 0..out_h {
        for c in 0..out_w {
            let src = ((r * 2) * w + (c * 2)) * ch;   // nearest-neighbour pick
            let dst = (r * out_w + c) * ch;
            buf[dst..dst + ch].copy_from_slice(&pixels[src..src + ch]);
        }
    }
    // Prevent the resize from being optimised away.
    unsafe { core::ptr::read_volatile(&buf[0]); }
}

/// Encode the next image from stream slot `in_slot` as binary PGM (P5)
/// and write it to I/O slot `out_io_slot` for the host `Output` node.
#[no_mangle]
pub extern "C" fn img_export_ppm(in_slot: u32, out_io_slot: u32) {
    let (_origin, data) = match ShmApi::read_next_stream_record(in_slot) { Some(r) => r, None => return };
    let (w, h, _ch, pixels) = match img_decode(&data) { Some(v) => v, None => return };
    let hdr = alloc::format!("P5\n{} {}\n255\n", w, h);
    let mut pgm = Vec::with_capacity(hdr.len() + w * h);
    pgm.extend_from_slice(hdr.as_bytes());
    pgm.extend_from_slice(&pixels[..w * h]);
    ShmApi::write_output_to(out_io_slot, &pgm);
}
