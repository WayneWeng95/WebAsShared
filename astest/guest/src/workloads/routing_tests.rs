// ─────────────────────────────────────────────────────────────────────────────
// Routing-test helpers
//
// These are called by the host routing-test roles (stream_bridge_test,
// aggregate_test, shuffle_test).  Each test:
//   1. Calls `produce_stream(id)` to populate upstream stream slots.
//   2. Performs host-side routing (Bridge / Aggregate / Shuffle) over SHM.
//   3. Calls `consume_routed_stream(id)` or `dump_stream_records(id)` to verify.
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use crate::api::ShmApi;

static mut READ_BUFFER: Vec<u8> = Vec::new();

/// Routing test (light): writes 3 labeled records to stream slot `id`.
#[no_mangle]
pub extern "C" fn produce_stream(id: u32) {
    for seq in 0..3u32 {
        let payload = alloc::format!("StreamPayload_P{}_seq{}", id, seq);
        ShmApi::append_stream_data(id, payload.as_bytes());
    }
}

/// Routing test (heavy): writes 150 structured records (~70 bytes each) to stream slot `id`.
/// Record format: "p={id},s={seq:04},DATA_BLOCK_{seq:04}_XXXXXXXXXXXXXXXXXXXXXXXXXXX"
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
#[no_mangle]
pub extern "C" fn count_stream_records(id: u32) -> u32 {
    ShmApi::read_all_stream_records(id).len() as u32
}

/// Returns the last complete record from stream slot `id` as a packed
/// `(ptr << 32 | len)` fat pointer, or `0` if the slot is empty.
#[no_mangle]
pub extern "C" fn consume_routed_stream(id: u32) -> u64 {
    if let Some((_origin, vec)) = ShmApi::read_latest_stream_data(id) {
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

/// Reads ALL records from stream slot `id`, joins them with `\n`, and returns a
/// packed `(ptr << 32 | len)` fat pointer into READ_BUFFER, or `0` if empty.
#[no_mangle]
pub extern "C" fn dump_stream_records(id: u32) -> u64 {
    let records = ShmApi::read_all_stream_records(id);
    if records.is_empty() { return 0; }
    let mut combined: Vec<u8> = Vec::new();
    for (i, (_origin, rec)) in records.iter().enumerate() {
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
