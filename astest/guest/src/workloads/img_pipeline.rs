// ─────────────────────────────────────────────────────────────────────────────
// Image processing pipeline demo  —  fixed-slot single-image pipeline
//
// Slot layout:
//   PIPE_IO_IN  (I/O 10)  ← host Input node writes raw PPM lines here
//   PIPE_LOAD   (str 20)  ← img_load_ppm   : parsed internal image
//   PIPE_ROT    (str 30)  ← img_rotate     : 90° CW rotated
//   PIPE_GRAY   (str 40)  ← img_grayscale  : grayscale
//   PIPE_EQ     (str 50)  ← img_equalize   : histogram equalised
//   PIPE_BLUR   (str 60)  ← img_blur       : box blurred
//   OUTPUT_IO_SLOT (I/O 1) ← img_export_ppm: binary PGM, flushed by Output node
//
// Internal image record format (one SHM record per image):
//   [width: u16 LE][height: u16 LE][channels: u8][pixels: w*h*ch bytes]
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

const PIPE_IO_IN: u32 = 10;
const PIPE_LOAD:  u32 = 20;
const PIPE_ROT:   u32 = 30;
const PIPE_GRAY:  u32 = 40;
const PIPE_EQ:    u32 = 50;
const PIPE_BLUR:  u32 = 60;

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

/// Load an ASCII PPM (P3) or PGM (P2) image from `PIPE_IO_IN` (I/O slot 10)
/// and write the internal binary image format to `PIPE_LOAD` (stream slot 20).
#[no_mangle]
pub extern "C" fn img_load_ppm(_: u32) {
    let lines = ShmApi::read_all_inputs_from(PIPE_IO_IN);

    let mut all = alloc::string::String::new();
    for (_origin, line) in &lines {
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
    ShmApi::append_stream_data(PIPE_LOAD, &data);
}

/// Rotate 90° CW: reads `PIPE_LOAD` (stream 20), writes to `PIPE_ROT` (stream 30).
#[no_mangle]
pub extern "C" fn img_rotate(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_LOAD);
    if records.is_empty() { return; }
    let (w, h, ch, pixels) = match img_decode(&records[0].1) { Some(v) => v, None => return };
    let (out_w, out_h) = (h, w);
    let mut buf = vec![0u8; out_w * out_h * ch];
    for r in 0..out_h {
        for c in 0..out_w {
            let src = ((h - 1 - c) * w + r) * ch;
            let dst = (r * out_w + c) * ch;
            buf[dst..dst + ch].copy_from_slice(&pixels[src..src + ch]);
        }
    }
    let mut data = Vec::with_capacity(5 + buf.len());
    data.extend_from_slice(&img_header(out_w, out_h, ch));
    data.extend_from_slice(&buf);
    ShmApi::append_stream_data(PIPE_ROT, &data);
}

/// Grayscale: reads `PIPE_ROT` (stream 30), writes to `PIPE_GRAY` (stream 40).
#[no_mangle]
pub extern "C" fn img_grayscale(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_ROT);
    if records.is_empty() { return; }
    let (w, h, ch, pixels) = match img_decode(&records[0].1) { Some(v) => v, None => return };
    let mut data = Vec::with_capacity(5 + w * h);
    data.extend_from_slice(&img_header(w, h, 1));
    for i in 0..w * h {
        let b = i * ch;
        let gray = if ch >= 3 {
            ((pixels[b] as u32 * 77 + pixels[b+1] as u32 * 150 + pixels[b+2] as u32 * 29) >> 8) as u8
        } else { pixels[b] };
        data.push(gray);
    }
    ShmApi::append_stream_data(PIPE_GRAY, &data);
}

/// Histogram equalisation: reads `PIPE_GRAY` (stream 40), writes to `PIPE_EQ` (stream 50).
#[no_mangle]
pub extern "C" fn img_equalize(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_GRAY);
    if records.is_empty() { return; }
    let (w, h, _ch, pixels) = match img_decode(&records[0].1) { Some(v) => v, None => return };
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
    let mut data = Vec::with_capacity(5 + total);
    data.extend_from_slice(&records[0].1[..5]);
    for &p in &pixels[..total] { data.push(lut[p as usize]); }
    ShmApi::append_stream_data(PIPE_EQ, &data);
}

/// 3×3 box blur: reads `PIPE_EQ` (stream 50), writes to `PIPE_BLUR` (stream 60).
#[no_mangle]
pub extern "C" fn img_blur(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_EQ);
    if records.is_empty() { return; }
    let (w, h, _ch, pixels) = match img_decode(&records[0].1) { Some(v) => v, None => return };
    let mut data = Vec::with_capacity(5 + w * h);
    data.extend_from_slice(&records[0].1[..5]);
    for r in 0..h {
        for c in 0..w {
            let mut sum = 0u32; let mut cnt = 0u32;
            for nr in r.saturating_sub(1)..=(r + 1).min(h - 1) {
                for nc in c.saturating_sub(1)..=(c + 1).min(w - 1) {
                    sum += pixels[nr * w + nc] as u32; cnt += 1;
                }
            }
            data.push((sum / cnt) as u8);
        }
    }
    ShmApi::append_stream_data(PIPE_BLUR, &data);
}

/// Encode the blurred image from `PIPE_BLUR` (stream 60) as binary PGM (P5)
/// and write it to `OUTPUT_IO_SLOT` for the host `Output` node to flush to disk.
#[no_mangle]
pub extern "C" fn img_export_ppm(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_BLUR);
    if records.is_empty() { return; }
    let (w, h, _ch, pixels) = match img_decode(&records[0].1) { Some(v) => v, None => return };
    let hdr = alloc::format!("P5\n{} {}\n255\n", w, h);
    let mut pgm = Vec::with_capacity(hdr.len() + w * h);
    pgm.extend_from_slice(hdr.as_bytes());
    pgm.extend_from_slice(&pixels[..w * h]);
    ShmApi::write_output(&pgm);
}
