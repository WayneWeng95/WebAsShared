// Final output channel: the last worker in a pipeline writes its results here,
// and the host Outputer saves them to a file after the node completes.
//
// Design: the guest writes records into the reserved `OUTPUT_SLOT_ID` stream
// slot, exactly like any other stream slot.  The host `Outputer` then reads
// that slot via the normal page-chain mechanism — no host imports, no special
// ABI.  The separation of concerns is:
//
//   guest  — decides WHAT to output and WHEN (calls `write_output` / `write_output_str`)
//   host   — decides WHERE to save it (the DAG `Output` node carries the file path)
//
// Multiple calls to `write_output` within a single worker invocation each
// append one record; the Outputer writes them all in order, one per line.
//
// Usage:
//   // At the end of the final processing stage, emit the result:
//   ShmApi::write_output_str(&format!("total_processed={}", count));

use common::OUTPUT_SLOT_ID;
use super::ShmApi;

impl ShmApi {
    /// Append `data` as a final output record to the reserved output slot.
    ///
    /// The host-side `Outputer` reads this slot after the producing node
    /// finishes and writes all accumulated records to the configured file path.
    /// Multiple calls append additional records; they are saved in call order.
    pub fn write_output(data: &[u8]) {
        Self::append_stream_data(OUTPUT_SLOT_ID, data);
    }

    /// Append a UTF-8 string as a final output record.
    ///
    /// Convenience wrapper around `write_output` for text results.
    pub fn write_output_str(s: &str) {
        Self::write_output(s.as_bytes());
    }
}
