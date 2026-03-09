// Host-side 1→1 stream bridge over shared memory.
// Points the downstream worker's head/tail at the upstream's existing page
// chain — zero copy, two atomic stores.
//
// For N→M routing with data movement see shuffle.rs.

use std::sync::atomic::Ordering;
use common::*;

pub struct HostStream {
    pub(crate) base: usize, // = splice_addr
}

impl HostStream {
    pub fn new(splice_addr: usize) -> Self {
        Self { base: splice_addr }
    }

    fn superblock(&self) -> &Superblock {
        unsafe { &*(self.base as *const Superblock) }
    }

    /// Wire a 1→1 connection: point `downstream_id`'s head/tail at
    /// `upstream_id`'s page chain. No data is copied.
    /// Returns `false` if upstream has not written anything yet.
    pub fn bridge(&self, upstream_id: usize, downstream_id: usize) -> bool {
        let sb = self.superblock();
        if upstream_id >= sb.writer_heads.len() || downstream_id >= sb.writer_heads.len() {
            return false;
        }
        let head = sb.writer_heads[upstream_id].load(Ordering::Acquire);
        if head == 0 { return false; }
        let tail = sb.writer_tails[upstream_id].load(Ordering::Acquire);
        sb.writer_heads[downstream_id].store(head, Ordering::Release);
        sb.writer_tails[downstream_id].store(tail, Ordering::Release);
        true
    }
}
