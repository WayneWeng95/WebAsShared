// Input/Output channels for the guest ↔ host data boundary.
//
// Both directions use the dedicated I/O area (io_heads / io_tails in the
// Superblock), which is completely separate from the stream slots used by
// inter-worker pipelines.  This means all STREAM_SLOT_COUNT stream slots are
// free for application use — none are reserved.
//
// OUTPUT — final results written by a pipeline stage:
//   The guest appends records to an I/O slot; the host `Outputer` reads that
//   slot after the producing DAG node completes and saves the records to a file.
//
//   · write_output(data)              — append to OUTPUT_IO_SLOT
//   · write_output_str(s)             — same, UTF-8 convenience
//   · write_output_to(slot, data)     — append to an explicit I/O slot
//   · write_output_str_to(slot, s)    — same, UTF-8 convenience
//
// INPUT — host-loaded file data consumed by the guest:
//   The host `Inputer` writes each line of a loaded file as one record into an
//   I/O slot before calling the WASM function.  The guest reads here.
//
//   · read_input()                    — latest record from INPUT_IO_SLOT
//   · read_all_inputs()               — all records from INPUT_IO_SLOT
//   · read_input_str()                — latest record as UTF-8 from INPUT_IO_SLOT
//   · read_input_from(slot)           — latest record from an explicit I/O slot
//   · read_all_inputs_from(slot)      — all records from an explicit I/O slot
//   · read_input_str_from(slot)       — latest record as UTF-8 from an explicit I/O slot

extern crate alloc;

use common::{INPUT_IO_SLOT, OUTPUT_IO_SLOT};
use super::ShmApi;

// ─── Output ──────────────────────────────────────────────────────────────────

impl ShmApi {
    /// Append `data` as an output record to the default `OUTPUT_IO_SLOT`.
    pub fn write_output(data: &[u8]) {
        Self::append_io_data(OUTPUT_IO_SLOT, data);
    }

    /// Append a UTF-8 string as an output record to the default `OUTPUT_IO_SLOT`.
    pub fn write_output_str(s: &str) {
        Self::write_output(s.as_bytes());
    }

    /// Append `data` as an output record to an explicit I/O `slot`.
    ///
    /// Use this when the DAG `Output` node targets a slot other than
    /// `OUTPUT_IO_SLOT` (e.g. when multiple workers each emit to their own
    /// output channel).
    pub fn write_output_to(slot: u32, data: &[u8]) {
        Self::append_io_data(slot, data);
    }

    /// Append a UTF-8 string as an output record to an explicit I/O `slot`.
    pub fn write_output_str_to(slot: u32, s: &str) {
        Self::write_output_to(slot, s.as_bytes());
    }
}

// ─── Input ───────────────────────────────────────────────────────────────────

impl ShmApi {
    /// Read the most recent record from the default `INPUT_IO_SLOT`.
    pub fn read_input() -> Option<alloc::vec::Vec<u8>> {
        Self::read_input_from(INPUT_IO_SLOT)
    }

    /// Read every record from the default `INPUT_IO_SLOT` in order.
    pub fn read_all_inputs() -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
        Self::read_all_inputs_from(INPUT_IO_SLOT)
    }

    /// Read the most recent input record as UTF-8 from the default `INPUT_IO_SLOT`.
    pub fn read_input_str() -> Option<alloc::string::String> {
        Self::read_input_str_from(INPUT_IO_SLOT)
    }

    /// Read the most recent record from an explicit I/O `slot`.
    pub fn read_input_from(slot: u32) -> Option<alloc::vec::Vec<u8>> {
        Self::read_latest_io_data(slot)
    }

    /// Read every record from an explicit I/O `slot` in order.
    pub fn read_all_inputs_from(slot: u32) -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
        Self::read_all_io_records(slot)
    }

    /// Read the most recent record as UTF-8 from an explicit I/O `slot`.
    pub fn read_input_str_from(slot: u32) -> Option<alloc::string::String> {
        Self::read_input_from(slot).and_then(|b| alloc::string::String::from_utf8(b).ok())
    }
}
