// Fan-out helper: write the same record to multiple stream slots in one call.
//
// This is the guest-side complement to the host `BroadcastConnection`. Where
// `BroadcastConnection` splices already-written page chains from N upstream
// slots into M downstream slots at the host routing layer, `fan_out` lets a
// single worker write identical data into several output slots directly —
// useful when one computation produces results that multiple downstream workers
// all need to see.
//
// Usage:
//   // Write the same encoded result to slots 10, 11, and 12 simultaneously.
//   ShmApi::fan_out(&[10, 11, 12], encoded_result.as_bytes());

use super::ShmApi;

impl ShmApi {
    /// Append `data` as a length-prefixed record to every slot in `slots`.
    ///
    /// Records are written in slot order.  Each call to `append_stream_data`
    /// is independent, so this is equivalent to calling it once per slot —
    /// the benefit is ergonomics: the caller doesn't need to write the loop.
    ///
    /// # Example
    /// ```ignore
    /// // Produce a result visible to three downstream workers on slots 5, 6, 7.
    /// ShmApi::fan_out(&[5, 6, 7], b"result_payload");
    /// ```
    pub fn fan_out(slots: &[u32], data: &[u8]) {
        for &slot in slots {
            Self::append_stream_data(slot, data);
        }
    }
}
