// Use for a easy stream setup, but how to actually use it need some adiitional thought, the API input and out may need to change later

use alloc::vec::Vec;
use super::ShmApi;

/// Connects two sandboxes via a shared stream: reads from `upstream_id`'s output
/// and writes to `self_id`'s output.
///
/// Usage:
/// ```ignore
/// let pipe = StreamPipe::new(upstream_id, self_id);
///
/// // Simple passthrough
/// pipe.pipe(|data| data.to_vec());
///
/// // Or step-by-step
/// if let Some(input) = pipe.read_input() {
///     let output = process(&input);
///     pipe.write_output(&output);
/// }
/// ```
pub struct StreamPipe {
    upstream_id: u32,
    self_id: u32,
}

impl StreamPipe {
    /// Creates a pipe that reads from `upstream_id` and writes to `self_id`.
    pub fn new(upstream_id: u32, self_id: u32) -> Self {
        Self { upstream_id, self_id }
    }

    /// Reads the latest record from the upstream sandbox's stream.
    /// Returns `None` if the upstream has not written any data yet.
    pub fn read_input(&self) -> Option<Vec<u8>> {
        ShmApi::read_latest_stream_data(self.upstream_id)
    }

    /// Writes `data` to this sandbox's own output stream.
    pub fn write_output(&self, data: &[u8]) {
        ShmApi::append_stream_data(self.self_id, data);
    }

    /// Reads the latest input, applies `transform`, and writes the result to the output stream.
    /// Returns `true` if input was available and output was written, `false` if no input yet.
    pub fn pipe<F: Fn(&[u8]) -> Vec<u8>>(&self, transform: F) -> bool {
        match self.read_input() {
            Some(input) => { self.write_output(&transform(&input)); true }
            None        => false,
        }
    }
}
