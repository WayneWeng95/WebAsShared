// N→M broadcast connection: fans every upstream into every downstream slot.

use super::shm_io::ShmIo;

/// Routes every upstream stream to every downstream slot.
///
/// Each downstream slot receives the complete merged chain of all upstreams,
/// producing a full N×M fan-out.  Downstream slots are processed sequentially
/// to avoid concurrent writes to the upstream tail-page `next_offset` fields
/// (multiple parallel merges of the same upstreams would race on those fields).
///
/// # Example
/// With upstreams [0, 1] and downstreams [2, 3]:
///   slot 2 receives pages from stream 0 followed by stream 1
///   slot 3 receives pages from stream 0 followed by stream 1
pub struct BroadcastConnection {
    upstream_ids: Vec<usize>,
    downstream_ids: Vec<usize>,
}

impl BroadcastConnection {
    pub fn new(upstream_ids: &[usize], downstream_ids: &[usize]) -> Self {
        Self {
            upstream_ids: upstream_ids.to_vec(),
            downstream_ids: downstream_ids.to_vec(),
        }
    }

    /// Merge all upstreams into each downstream slot in turn.
    pub fn bridge(&self, splice_addr: usize) {
        let io = ShmIo::new(splice_addr);
        for &dst_id in &self.downstream_ids {
            io.merge_into(&self.upstream_ids, dst_id);
        }
    }
}
