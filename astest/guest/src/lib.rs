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
// Word count demo
//
// Two-stage map-reduce pipeline driven by the DAG runner:
//
//   Input node   → loads data/sample.txt into I/O slot 0 (one record per line)
//   wc_map(0)    → reads each line from I/O slot 0, counts words, appends
//                  "line=N,words=M" records to stream slot 10
//   wc_reduce(10)→ reads all stream records from slot 10, accumulates totals,
//                  writes "total_lines=N,total_words=M" to OUTPUT_IO_SLOT
//   Output node  → flushes OUTPUT_IO_SLOT to /tmp/word_count_result.txt
// ─────────────────────────────────────────────────────────────────────────────

const WC_STREAM_SLOT: u32 = 10;

/// Map stage: read every line from I/O slot `io_slot`, count words per line,
/// and append one `"line=N,words=M"` record to `WC_STREAM_SLOT`.
#[no_mangle]
pub extern "C" fn wc_map(io_slot: u32) {
    let lines = ShmApi::read_all_inputs_from(io_slot);
    for (n, line) in lines.iter().enumerate() {
        let words = core::str::from_utf8(line)
            .unwrap_or("")
            .split_whitespace()
            .count();
        let rec = alloc::format!("line={},words={}", n, words);
        ShmApi::append_stream_data(WC_STREAM_SLOT, rec.as_bytes());
    }
}

/// Reduce stage: read all `"line=N,words=M"` records from `stream_slot`,
/// sum the word counts, and write one `"total_lines=N,total_words=M"` record
/// to `OUTPUT_IO_SLOT`.
#[no_mangle]
pub extern "C" fn wc_reduce(stream_slot: u32) {
    let records = ShmApi::read_all_stream_records(stream_slot);
    let total_lines = records.len();
    let mut total_words: u64 = 0;
    for rec in &records {
        if let Ok(s) = core::str::from_utf8(rec) {
            // format: "line=N,words=M"
            if let Some(words_part) = s.split(',').nth(1) {
                if let Some(val) = words_part.split('=').nth(1) {
                    if let Ok(n) = val.parse::<u64>() {
                        total_words += n;
                    }
                }
            }
        }
    }
    ShmApi::write_output_str(&alloc::format!(
        "total_lines={},total_words={}",
        total_lines, total_words
    ));
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