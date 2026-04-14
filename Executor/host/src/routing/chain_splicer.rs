// Internal SHM page-chain splice primitive shared by all routing connection types.
//
// ChainSplicer wraps a splice_addr (the SHM superblock base) and provides the
// two building blocks every connection needs:
//   chain_onto  — O(1) splice: link src's page chain onto the end of dst's.
//   merge_into  — merge N upstream chains into one dst; sequential for
//                 N ≤ PARALLEL_THRESHOLD, parallel tree-merge beyond that.
//
// ChainSplicer is Copy (one usize) so it can be moved into thread closures for free.

use std::sync::atomic::Ordering;
use std::thread;
use common::{Page, ShmOffset, Superblock, PARALLEL_THRESHOLD};

#[derive(Clone, Copy)]
pub(super) struct ChainSplicer {
    base: usize,
}

// Safe: all mutations go through atomics on shared memory that both host
// and guest already access concurrently.
unsafe impl Send for ChainSplicer {}
unsafe impl Sync for ChainSplicer {}

impl ChainSplicer {
    pub(super) fn new(splice_addr: usize) -> Self {
        Self { base: splice_addr }
    }

    pub(super) fn superblock(&self) -> &Superblock {
        unsafe { &*(self.base as *const Superblock) }
    }

    fn page_at_mut(&self, offset: ShmOffset) -> &mut Page {
        unsafe { &mut *((self.base + offset as usize) as *mut Page) }
    }

    /// Splice `src_id`'s chain onto the end of `dst_id`'s chain. O(1):
    /// writer_tails[dst_id] already holds the tail page — no walking needed.
    pub(super) fn chain_onto(&self, dst_id: usize, src_id: usize) {
        let sb = self.superblock();
        let src_head = sb.writer_heads[src_id].load(Ordering::Acquire) as ShmOffset;
        if src_head == 0 { return; }
        let src_tail = sb.writer_tails[src_id].load(Ordering::Acquire) as ShmOffset;

        let dst_tail = sb.writer_tails[dst_id].load(Ordering::Acquire) as ShmOffset;
        if dst_tail == 0 {
            sb.writer_heads[dst_id].store(src_head as u64, Ordering::Release);
        } else {
            self.page_at_mut(dst_tail).next_offset.store(src_head as u64, Ordering::Release);
        }
        sb.writer_tails[dst_id].store(src_tail as u64, Ordering::Release);
    }

    /// Merge `upstream_ids` into `dst_id`.
    /// Sequential for N ≤ PARALLEL_THRESHOLD, parallel tree merge beyond that.
    pub(super) fn merge_into(&self, upstream_ids: &[usize], dst_id: usize) {
        if upstream_ids.is_empty() { return; }

        if upstream_ids.len() <= PARALLEL_THRESHOLD {
            for &up in upstream_ids {
                self.chain_onto(dst_id, up);
            }
        } else {
            // Parallel tree merge.
            // Each level chains adjacent pairs; pairs in the same level touch
            // disjoint superblock entries and tail pages, so they run concurrently.
            //
            //   [u0, u1, u2, u3, u4, u5, u6, u7]
            // L0 (parallel): u0←u1  u2←u3  u4←u5  u6←u7
            // L1 (parallel): u0←u2  u4←u6
            // L2:            u0←u4
            // → point dst at u0

            let mut active: Vec<usize> = upstream_ids.to_vec();

            while active.len() > 1 {
                let pairs: Vec<(usize, usize)> = active
                    .chunks(2)
                    .filter_map(|c| if c.len() == 2 { Some((c[0], c[1])) } else { None })
                    .collect();

                thread::scope(|s| {
                    for &(pair_dst, pair_src) in &pairs {
                        s.spawn(move || self.chain_onto(pair_dst, pair_src));
                    }
                });

                active = active.into_iter().step_by(2).collect();
            }

            self.chain_onto(dst_id, active[0]);
        }
    }
}
