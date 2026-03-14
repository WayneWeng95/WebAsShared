extern crate alloc;
mod api;

use core::sync::atomic::Ordering;
use api::ShmApi; 
use alloc::format;
use alloc::vec::Vec;

/// WASM export: writer workload for worker `id`.
/// Updates named atomics in the Registry, inserts a conflict node into the shared hash map,
/// and appends a JSON summary record to the worker's private stream.
#[no_mangle]
pub extern "C" fn writer(id: u32) {
    // ShmApi::append_log(&format!(">>> [INFO] Writer {} initialized.\n", id));

    // [Plan A] 1. Distinct Global Counter for Requests (+1)
    // The Registry will assign a unique index for "TotalRequests" (likely Index 0)
    let global_reqs = ShmApi::get_named_atomic("TotalRequests");
    let current_reqs = global_reqs.fetch_add(1, Ordering::SeqCst);

    // [Heap Logic] Simulate local computation
    let mut private_heap_data: Vec<u64> = Vec::new();
    for i in 1..=5 {
        private_heap_data.push((id as u64 * 1000) + i);
    }
    let local_sum: u64 = private_heap_data.iter().sum();

    // [Plan A] 2. Distinct Local Counter
    // Instead of hardcoding (id + 1), use a dynamic name!
    // Example: "Worker_0_Status", "Worker_1_Status"...
    let local_name = format!("Worker_{}_Status", id);
    let local_counter = ShmApi::get_named_atomic(&local_name);
    let local_val = local_counter.fetch_add(1, Ordering::Relaxed);

    // [Plan A] 3. Distinct Global Batch Counter (+100)
    // This uses a DIFFERENT name, so Registry assigns a DIFFERENT index (likely Index 1 + 4 workers...)
    // No more aliasing with "TotalRequests"!
    let global_batch = ShmApi::get_named_atomic("GlobalBatchCounter");
    let batch_val = global_batch.fetch_add(100, Ordering::SeqCst);

    // 2. [Test] Collision on Key 888 (in Dynamic Page Pool Map)
    let shared_key = 888; 
    let conflict_data = format!("Dynamic_Chain_Data_From_W{}", id);
    ShmApi::insert_shared_data(shared_key, id, conflict_data.as_bytes());
    
    // Construct Payload with clearly separated metrics
    let complex_data = format!(
        r#"{{"worker_id": {}, "local_status": {}, "global_reqs": {}, "global_batch": {}, "local_sum": {}}}"#, 
        id, local_val, current_reqs, batch_val, local_sum
    );

    ShmApi::append_stream_data(id, complex_data.as_bytes());

    // ShmApi::append_log(&format!("<<< [SUCCESS] Writer {} completed.\n", id));
}

// Persistent buffer used to extend the lifetime of the last read payload across the ABI boundary.
static mut READ_BUFFER: Vec<u8> = Vec::new();

/// WASM export: reads the latest record from writer `id`'s private stream.
/// Returns a packed `(ptr << 32 | len)` fat pointer for the host to read directly,
/// or `0` if no data is available yet.
#[no_mangle]
pub extern "C" fn reader(id: u32) -> u64 {
    if let Some(vec) = ShmApi::read_latest_stream_data(id) {
        unsafe {
            // take the read buffer
            READ_BUFFER = vec;
            // get the fat pointer
            let ptr = READ_BUFFER.as_ptr() as u64;
            let len = READ_BUFFER.len() as u64;
            (ptr << 32) | len
        }
    } else {
        0
    }
}

/// WASM export: returns the current value of registry atomic index 0 (`TotalRequests`).
#[no_mangle]
pub extern "C" fn read_live_global() -> u64 {
    ShmApi::get_atomic(0).load(Ordering::SeqCst)
}

/// WASM export: writes a finalized result string under the `"FuncA_Result"` task name.
/// Competing invocations from multiple workers are resolved by the Manager's conflict policy.
#[no_mangle]
pub extern "C" fn func_a(id: u32) {
    let result = alloc::format!("This is the finalized data from Function A! (Winner ID: {})", id);
    
    ShmApi::write_shared_state("FuncA_Result", id, result.as_bytes());
    ShmApi::append_log(&alloc::format!("Func A (ID: {}) wrote output.\n", id));
}

/// WASM export: reads the Manager-committed result of `"FuncA_Result"` and logs it.
/// Logs both the UTF-8 text and a hex dump of the first 16 bytes to the shared log arena.
#[no_mangle]
pub extern "C" fn func_b(_id: u32) {
    if let Some(input_data) = ShmApi::read_shared_state("FuncA_Result") {
        let text = alloc::string::String::from_utf8_lossy(&input_data);
        
       
        ShmApi::append_log(&alloc::format!("Func B received (len: {}): {}\n", input_data.len(), text));
        
        
        let mut hex_str = alloc::string::String::new();
        for &b in input_data.iter().take(16) {
            hex_str.push_str(&alloc::format!("{:02X} ", b));
        }
        ShmApi::append_log(&alloc::format!("Hex Dump (first 16 bytes): {}\n", hex_str));
        
    } else {
        ShmApi::append_log("Func B found no input!\n");
    }
}

/// Node A: processes images and produces both "processing result (Stream)" and "global state stats (Shared)"
#[no_mangle]
pub extern "C" fn process_image_node(id: u32) {
    // 1. Produce large business output (via Stream channel, no Manager involvement)
    let image_result = b"Binary_Image_Data...";
    ShmApi::append_stream_data(id, image_result);

    // 2. Report global task progress (via Shared channel, multi-Worker concurrent writes, Manager resolves LWW)
    let progress_msg = format!("Worker {} finished batch.", id);
    ShmApi::write_shared_state("Global_Job_Status", id, progress_msg.as_bytes());
}

/// Node B: packs Node A's output; it needs to read both kinds of data above
#[no_mangle]
pub extern "C" fn zip_results_node(id: u32) {
    // 1. Read Node A's (assume id 1) private output (directly from in-memory list, zero wait)
    if let Some(img_data) = ShmApi::read_latest_stream_data(1) {
        // ... pack img_data ...
    }

    // 2. Read global task status (from Registry, Manager-confirmed data)
    if let Some(status) = ShmApi::read_shared_state("Global_Job_Status") {
        // ... check whether all tasks are done ...
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Word count demo  —  parallel map-reduce (10 workers)
//
// Slot layout:
//   I/O  slot 0          : raw input lines (written by host Input node)
//   stream slots 10–19   : per-worker line partitions (written by wc_distribute)
//   stream slots 110–119 : per-worker word-frequency records (written by wc_map)
//   stream slot  200     : merged mapper output (written by host Aggregate node)
//
// DAG stages:
//   Input          → load corpus into I/O slot 0
//   wc_distribute  → fan lines round-robin into stream slots 10–19
//   wc_map × 10   → each worker counts words in its slot, writes to slot+100
//   Aggregate      → merge stream slots 110–119 → slot 200
//   wc_reduce      → aggregate per-word counts from slot 200, write to output
//   Output         → flush output I/O slot to file
//
// Record formats:
//   wc_map  emits : "word=<w>\x1f<count>" (0x1F unit-separator avoids comma
//                    clash with words containing none, and is never in plain text)
//   wc_reduce emits: "<word>: <count>"  (one line per unique word, sorted)
//                    plus a header record "=== word_count ==="
// ─────────────────────────────────────────────────────────────────────────────

/// First stream slot used by the distribute stage.
const WC_DIST_BASE: u32 = 10;
/// Map output base: wc_map(slot) writes to stream slot `slot + WC_MAP_OUT_BASE`.
const WC_MAP_OUT_BASE: u32 = 100;

/// Distribute: read every line from the default input I/O slot and append it
/// round-robin to one of the `n_workers` stream slots starting at `WC_DIST_BASE`.
/// Worker i receives all lines where `line_index % n_workers == i`.
#[no_mangle]
pub extern "C" fn wc_distribute(n_workers: u32) {
    let lines = ShmApi::read_all_inputs();
    for (i, line) in lines.iter().enumerate() {
        let slot = WC_DIST_BASE + (i as u32 % n_workers);
        ShmApi::append_stream_data(slot, line);
    }
}

/// Map: count occurrences of every unique word in stream slot `slot`.
/// Each word is lower-cased and stripped of non-alphabetic characters before
/// counting so "Rust," and "rust" tally together.
/// Emits one `"word=<w>\x1f<count>"` record per unique word to stream slot
/// `slot + WC_MAP_OUT_BASE`.
#[no_mangle]
pub extern "C" fn wc_map(slot: u32) {
    // Collect word → count pairs into a sorted vec (BTreeMap-style via insertion sort
    // on a flat Vec so we stay alloc-only with no std dependency).
    let mut counts: Vec<(alloc::string::String, u64)> = Vec::new();

    let records = ShmApi::read_all_stream_records(slot);
    for rec in &records {
        let line = core::str::from_utf8(rec).unwrap_or("");
        for token in line.split_whitespace() {
            let word: alloc::string::String = token
                .chars()
                .filter(|c| c.is_alphabetic())
                .map(|c| {
                    if c >= 'A' && c <= 'Z' { (c as u8 + 32) as char } else { c }
                })
                .collect();
            if word.is_empty() { continue; }
            // Linear scan — acceptable for per-chunk vocabulary sizes.
            match counts.iter_mut().find(|(w, _)| w == &word) {
                Some((_, n)) => *n += 1,
                None => counts.push((word, 1)),
            }
        }
    }

    let out_slot = slot + WC_MAP_OUT_BASE;
    for (word, count) in &counts {
        // Use ASCII unit-separator (0x1F) as field delimiter — never appears in text.
        let rec = alloc::format!("word={}\x1f{}", word, count);
        ShmApi::append_stream_data(out_slot, rec.as_bytes());
    }
}

/// Reduce: read all `"word=<w>\x1f<count>"` records from `stream_slot`,
/// merge counts across all mappers, sort alphabetically, and write one
/// `"<word>: <count>"` record per unique word plus a summary header to
/// `OUTPUT_IO_SLOT`.
#[no_mangle]
pub extern "C" fn wc_reduce(stream_slot: u32) {
    let mut totals: Vec<(alloc::string::String, u64)> = Vec::new();

    let records = ShmApi::read_all_stream_records(stream_slot);
    for rec in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        // format: "word=<w>\x1f<count>"
        let body = match s.strip_prefix("word=") {
            Some(b) => b,
            None => continue,
        };
        let sep = match body.find('\x1f') {
            Some(i) => i,
            None => continue,
        };
        let word = &body[..sep];
        let count: u64 = body[sep + 1..].parse().unwrap_or(0);
        if word.is_empty() { continue; }
        match totals.iter_mut().find(|(w, _)| w == word) {
            Some((_, n)) => *n += count,
            None => totals.push((alloc::string::String::from(word), count)),
        }
    }

    // Sort alphabetically.
    totals.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

    let unique = totals.len();
    let total: u64 = totals.iter().map(|(_, n)| n).sum();

    ShmApi::write_output_str("=== word_count ===");
    ShmApi::write_output_str(&alloc::format!("unique_words={}", unique));
    ShmApi::write_output_str(&alloc::format!("total_occurrences={}", total));
    for (word, count) in &totals {
        ShmApi::write_output_str(&alloc::format!("{}: {}", word, count));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Image processing pipeline demo  —  4 synthetic images processed in parallel
//
// Each image travels through the same 5 processing stages independently so all
// 4 pipelines run in parallel at every wave.
//
// Stages and slot layout:
//   img_generate(arg)  : arg = 10-13 → write RGB  128×128 to stream slot arg
//   img_rotate(arg)    : arg = 10-13 → read slot arg, write 90°CW to arg+10
//   img_grayscale(arg) : arg = 20-23 → read slot arg, write gray to arg+10
//   img_equalize(arg)  : arg = 30-33 → read slot arg, write equalized to arg+10
//   img_blur(arg)      : arg = 40-43 → read slot arg, write blurred to arg+10
//   img_export(i)      : i = 0-3     → read stream 50+i, write PGM to I/O slot 2+i
//
// Internal image record format  (one SHM record per image):
//   [width: u16 LE][height: u16 LE][channels: u8][pixels: w*h*ch bytes]
//   channels = 3 (RGB) for generate/rotate, 1 (gray) thereafter.
//
// Output: binary PGM (P5) files, viewable with any image tool that reads PGM.
// ─────────────────────────────────────────────────────────────────────────────

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

// ─── Fixed-slot image pipeline ───────────────────────────────────────────────
//
// Every function in this pipeline uses the same hard-coded slot numbers so the
// DAG JSON can call them with `"arg": 0` (ignored) for every image.
// A host `FreeSlots` node resets these slots between images so the pipeline
// can be reused without any slot-number offsets.
//
//   PIPE_IO_IN  (I/O 10)  ← host Input node writes raw PPM lines here
//   PIPE_LOAD   (str 20)  ← img_load_ppm   : parsed internal image
//   PIPE_ROT    (str 30)  ← img_rotate     : 90° CW rotated
//   PIPE_GRAY   (str 40)  ← img_grayscale  : grayscale
//   PIPE_EQ     (str 50)  ← img_equalize   : histogram equalised
//   PIPE_BLUR   (str 60)  ← img_blur       : box blurred
//   OUTPUT_IO_SLOT (I/O 1) ← img_export_ppm: binary PGM, flushed by Output node
// ─────────────────────────────────────────────────────────────────────────────

const PIPE_IO_IN: u32 = 10;
const PIPE_LOAD:  u32 = 20;
const PIPE_ROT:   u32 = 30;
const PIPE_GRAY:  u32 = 40;
const PIPE_EQ:    u32 = 50;
const PIPE_BLUR:  u32 = 60;

/// Load an ASCII PPM (P3) or PGM (P2) image from `PIPE_IO_IN` (I/O slot 10)
/// and write the internal binary image format to `PIPE_LOAD` (stream slot 20).
/// The `arg` parameter is unused; pass `0` in the DAG JSON.
#[no_mangle]
pub extern "C" fn img_load_ppm(_: u32) {
    let lines = ShmApi::read_all_inputs_from(PIPE_IO_IN);

    let mut all = alloc::string::String::new();
    for line in &lines {
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
/// The `arg` parameter is unused; pass `0` in the DAG JSON.
#[no_mangle]
pub extern "C" fn img_rotate(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_LOAD);
    if records.is_empty() { return; }
    let (w, h, ch, pixels) = match img_decode(&records[0]) { Some(v) => v, None => return };
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
/// The `arg` parameter is unused; pass `0` in the DAG JSON.
#[no_mangle]
pub extern "C" fn img_grayscale(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_ROT);
    if records.is_empty() { return; }
    let (w, h, ch, pixels) = match img_decode(&records[0]) { Some(v) => v, None => return };
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
/// The `arg` parameter is unused; pass `0` in the DAG JSON.
#[no_mangle]
pub extern "C" fn img_equalize(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_GRAY);
    if records.is_empty() { return; }
    let (w, h, _ch, pixels) = match img_decode(&records[0]) { Some(v) => v, None => return };
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
    data.extend_from_slice(&records[0][..5]);
    for &p in &pixels[..total] { data.push(lut[p as usize]); }
    ShmApi::append_stream_data(PIPE_EQ, &data);
}

/// 3×3 box blur: reads `PIPE_EQ` (stream 50), writes to `PIPE_BLUR` (stream 60).
/// The `arg` parameter is unused; pass `0` in the DAG JSON.
#[no_mangle]
pub extern "C" fn img_blur(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_EQ);
    if records.is_empty() { return; }
    let (w, h, _ch, pixels) = match img_decode(&records[0]) { Some(v) => v, None => return };
    let mut data = Vec::with_capacity(5 + w * h);
    data.extend_from_slice(&records[0][..5]);
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
/// The `arg` parameter is unused; pass `0` in the DAG JSON.
#[no_mangle]
pub extern "C" fn img_export_ppm(_: u32) {
    let records = ShmApi::read_all_stream_records(PIPE_BLUR);
    if records.is_empty() { return; }
    let (w, h, _ch, pixels) = match img_decode(&records[0]) { Some(v) => v, None => return };
    let hdr = alloc::format!("P5\n{} {}\n255\n", w, h);
    let mut pgm = Vec::with_capacity(hdr.len() + w * h);
    pgm.extend_from_slice(hdr.as_bytes());
    pgm.extend_from_slice(&pixels[..w * h]);
    ShmApi::write_output(&pgm);
}

// ─────────────────────────────────────────────────────────────────────────────
// 4-stage streaming pipeline
//
// The host's `StreamPipeline` DAG node drives these functions in a loop.
// Each round the source appends a fresh batch; the downstream stages advance
// their own per-stage cursors (stored as named SHM atomics) so they consume
// only records that arrived since the last call.
//
// Slot layout (configurable via JSON):
//   source_slot    (200): raw batch records from the source
//   filter_slot    (201): even-item records kept by the filter stage
//   transform_slot (202): filtered records with "|T" appended
//   summary_slot   (203): one summary record per round from the sink
// ─────────────────────────────────────────────────────────────────────────────

const PIPELINE_BATCH: u32 = 20;

/// Stage 1 — source: appends `PIPELINE_BATCH` records to `out_slot`.
/// Record format: "r={round},i={item:02},v={value:05}" where value = round*1000+item.
#[no_mangle]
pub extern "C" fn pipeline_source(out_slot: u32, round: u32) {
    for i in 0..PIPELINE_BATCH {
        let v = round * 1000 + i;
        let rec = alloc::format!("r={},i={:02},v={:05}", round, i, v);
        ShmApi::append_stream_data(out_slot, rec.as_bytes());
    }
}

/// Stage 2 — filter: reads new records from `in_slot` (cursor: "pipe_filter_cursor"),
/// keeps only those with an even item index, appends surviving records to `out_slot`.
#[no_mangle]
pub extern "C" fn pipeline_filter(in_slot: u32, out_slot: u32) {
    let cursor = ShmApi::get_named_atomic("pipe_filter_cursor");
    let start = cursor.load(Ordering::Acquire) as usize;
    let all = ShmApi::read_all_stream_records(in_slot);
    let new_recs = &all[start.min(all.len())..];
    for rec in new_recs {
        // Format: "r=N,i=MM,v=VVVVV" — keep records where item index MM is even.
        let keep = core::str::from_utf8(rec).ok()
            .and_then(|s| s.split(',').nth(1))       // "i=MM"
            .and_then(|p| p.split('=').nth(1))        // "MM"
            .and_then(|n| n.parse::<u32>().ok())
            .map(|n| n % 2 == 0)
            .unwrap_or(false);
        if keep {
            ShmApi::append_stream_data(out_slot, rec);
        }
    }
    cursor.store((start + new_recs.len()) as u64, Ordering::Release);
}

/// Stage 3 — transform: reads new records from `in_slot` (cursor: "pipe_transform_cursor"),
/// appends the "|T" transformation marker, writes results to `out_slot`.
#[no_mangle]
pub extern "C" fn pipeline_transform(in_slot: u32, out_slot: u32) {
    let cursor = ShmApi::get_named_atomic("pipe_transform_cursor");
    let start = cursor.load(Ordering::Acquire) as usize;
    let all = ShmApi::read_all_stream_records(in_slot);
    let new_recs = &all[start.min(all.len())..];
    for rec in new_recs {
        let mut out = Vec::with_capacity(rec.len() + 2);
        out.extend_from_slice(rec);
        out.extend_from_slice(b"|T");
        ShmApi::append_stream_data(out_slot, &out);
    }
    cursor.store((start + new_recs.len()) as u64, Ordering::Release);
}

/// Stage 4 — sink: reads new records from `in_slot` (cursor: "pipe_sink_cursor"),
/// sums their value fields, appends a per-batch summary to `summary_slot`.
/// Summary format: "batch_count={N},value_sum={S}"
#[no_mangle]
pub extern "C" fn pipeline_sink(in_slot: u32, summary_slot: u32) {
    let cursor = ShmApi::get_named_atomic("pipe_sink_cursor");
    let start = cursor.load(Ordering::Acquire) as usize;
    let all = ShmApi::read_all_stream_records(in_slot);
    let new_recs = &all[start.min(all.len())..];
    let count = new_recs.len() as u32;
    if count > 0 {
        let mut value_sum: u64 = 0;
        for rec in new_recs {
            // Format after transform: "r=N,i=MM,v=VVVVV|T"
            if let Ok(s) = core::str::from_utf8(rec) {
                if let Some(v_part) = s.split(',').nth(2) {         // "v=VVVVV|T"
                    if let Some(v_str) = v_part.split('=').nth(1) { // "VVVVV|T"
                        let v_clean = v_str.trim_end_matches("|T");
                        if let Ok(v) = v_clean.parse::<u64>() {
                            value_sum += v;
                        }
                    }
                }
            }
        }
        let summary = alloc::format!("batch_count={},value_sum={}", count, value_sum);
        ShmApi::append_stream_data(summary_slot, summary.as_bytes());
    }
    cursor.store((start + new_recs.len()) as u64, Ordering::Release);
}

// ─────────────────────────────────────────────────────────────────────────────
// Fan-out and final output demos
//
// `produce_fan_out(id)` — writes one record to three SHM stream slots at once
//   using `ShmApi::fan_out`, demonstrating the guest-side multi-stream API.
//   Slot layout: id (primary), id+100 (secondary A), id+200 (secondary B).
//
// `write_final_output(id)` — appends one record to the reserved output slot
//   using `ShmApi::write_output_str`.  The host `Output` DAG node saves all
//   accumulated records to the configured file path after this node finishes.
// ─────────────────────────────────────────────────────────────────────────────

/// Fan-out demo: write the same record to three stream slots simultaneously.
///
/// Demonstrates `ShmApi::fan_out` — one payload, three destinations, no loop
/// in the caller.  The host can then route slots id+100 and id+200 to
/// downstream workers independently of the primary slot.
#[no_mangle]
pub extern "C" fn produce_fan_out(id: u32) {
    let payload = alloc::format!("fan_out_src={},v={}", id, id * id);
    ShmApi::fan_out(&[id, id + 100, id + 200], payload.as_bytes());
}

/// Final output demo: compute a result and write it to the reserved output slot.
///
/// Demonstrates `ShmApi::write_output_str`.  Multiple workers can call this;
/// all records accumulate in order and are flushed to disk by the `Output` DAG
/// node that depends on them.
#[no_mangle]
pub extern "C" fn write_final_output(id: u32) {
    ShmApi::write_output_str(&alloc::format!(
        "worker={},result={},squared={}",
        id, id * 10, id * id
    ));
}

// ─────────────────────────────────────────────────────────────────────────────
// Routing-test helpers
//
// These are called by the host routing-test roles (stream_bridge_test,
// aggregate_test, shuffle_test) in worker.rs.  Each test:
//   1. Calls `produce_stream(id)` to populate upstream stream slots with
//      identifiable, length-prefixed records.
//   2. Performs host-side routing (HostStream::bridge / AggregateConnection /
//      ShuffleConnection) over the shared-memory page chains.
//   3. Calls `consume_routed_stream(id)` on the downstream slot to verify
//      that the expected data arrived.
// ─────────────────────────────────────────────────────────────────────────────

/// Routing test (light): writes 3 labeled records to stream slot `id`.
/// Used by the bridge and aggregate tests where a small record count is enough.
#[no_mangle]
pub extern "C" fn produce_stream(id: u32) {
    for seq in 0..3u32 {
        let payload = alloc::format!("StreamPayload_P{}_seq{}", id, seq);
        ShmApi::append_stream_data(id, payload.as_bytes());
    }
}

/// Routing test (heavy): writes 150 structured records (~70 bytes each) to stream
/// slot `id`, producing a multi-page chain (~2–3 pages of 4 KB each per slot).
/// Record format: "p={id},s={seq:04},DATA_BLOCK_{seq:04}_XXXXXXXXXXXXXXXXXXXXXXXXXXX"
/// The structured prefix makes producer origin and sequence unambiguous after routing.
#[no_mangle]
pub extern "C" fn produce_stream_heavy(id: u32) {
    for seq in 0..150u32 {
        let payload = alloc::format!(
            "p={},s={:04},DATA_BLOCK_{:04}_XXXXXXXXXXXXXXXXXXXXXXXXXXX",
            id, seq, seq
        );
        ShmApi::append_stream_data(id, payload.as_bytes());
    }
}

/// Returns the total number of length-prefixed records in stream slot `id`.
/// Used by the host after routing to verify expected record counts per downstream slot.
#[no_mangle]
pub extern "C" fn count_stream_records(id: u32) -> u32 {
    ShmApi::read_all_stream_records(id).len() as u32
}

/// Routing test: returns the last complete record from stream slot `id` as a
/// packed `(ptr << 32 | len)` fat pointer, or `0` if the slot is empty.
/// Used by the host after routing to verify the correct data arrived.
#[no_mangle]
pub extern "C" fn consume_routed_stream(id: u32) -> u64 {
    if let Some(vec) = ShmApi::read_latest_stream_data(id) {
        unsafe {
            READ_BUFFER = vec;
            let ptr = READ_BUFFER.as_ptr() as u64;
            let len = READ_BUFFER.len() as u64;
            (ptr << 32) | len
        }
    } else {
        0
    }
}

/// Routing test: reads ALL records from stream slot `id`, joins them with `\n`,
/// and returns a packed `(ptr << 32 | len)` fat pointer into READ_BUFFER.
/// Returns `0` if the slot is empty.
/// Used by aggregate and shuffle tests where the full ordered record list
/// (across all merged/routed chains) must be verified, not just the last one.
#[no_mangle]
pub extern "C" fn dump_stream_records(id: u32) -> u64 {
    let records = ShmApi::read_all_stream_records(id);
    if records.is_empty() { return 0; }
    let mut combined: Vec<u8> = Vec::new();
    for (i, rec) in records.iter().enumerate() {
        if i > 0 { combined.push(b'\n'); }
        combined.extend_from_slice(rec);
    }
    unsafe {
        READ_BUFFER = combined;
        let ptr = READ_BUFFER.as_ptr() as u64;
        let len = READ_BUFFER.len() as u64;
        (ptr << 32) | len
    }
}