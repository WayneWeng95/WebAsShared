// Host-side N→M shuffle via zero-copy page chain splicing.
//
// chain_onto is O(1): writer_tails[id] already holds the tail page offset,
// so no page-walking is needed. For N > PARALLEL_THRESHOLD upstreams per
// downstream slot, a parallel tree merge reduces the sequential dependency
// chain from O(N) to O(log N) levels, each level run concurrently.
//
// 1→1 connections: use HostStream::bridge directly (two atomic stores).

use std::sync::atomic::Ordering;
use std::thread;
use common::*;


// -----------------------------------------------------------------------------
// ShmIo — O(1) chain splicing
// -----------------------------------------------------------------------------

// ShmIo is just a usize (the shm base address). Copy is intentional:
// cloning into thread closures copies one pointer-sized integer — one instruction.
#[derive(Clone, Copy)]
struct ShmIo {
    base: usize,
}

// Safe: all mutations go through atomics on shared memory that both host
// and guest already access concurrently.
unsafe impl Send for ShmIo {}
unsafe impl Sync for ShmIo {}

impl ShmIo {
    fn new(splice_addr: usize) -> Self { Self { base: splice_addr } }

    fn superblock(&self) -> &Superblock {
        unsafe { &*(self.base as *const Superblock) }
    }

    fn page_at_mut(&self, offset: u32) -> &mut Page {
        unsafe { &mut *((self.base + offset as usize) as *mut Page) }
    }

    /// Splice `src_id`'s chain onto the end of `dst_id`'s chain. O(1):
    /// writer_tails[dst_id] is the tail page — no walking needed.
    fn chain_onto(&self, dst_id: usize, src_id: usize) {
        let sb = self.superblock();
        let src_head = sb.writer_heads[src_id].load(Ordering::Acquire);
        if src_head == 0 { return; }
        let src_tail = sb.writer_tails[src_id].load(Ordering::Acquire);

        let dst_tail = sb.writer_tails[dst_id].load(Ordering::Acquire);
        if dst_tail == 0 {
            // dst is empty: just point its head at src
            sb.writer_heads[dst_id].store(src_head, Ordering::Release);
        } else {
            // link dst's current tail page to src's head
            self.page_at_mut(dst_tail).next_offset.store(src_head, Ordering::Release);
        }
        sb.writer_tails[dst_id].store(src_tail, Ordering::Release);
    }

    /// Merge `upstream_ids` into `dst_id`.
    /// Sequential for N ≤ PARALLEL_THRESHOLD, parallel tree merge beyond that.
    fn merge_into(&self, upstream_ids: &[usize], dst_id: usize) {
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

                // Each pair (dst, src) touches writer_tails[dst]'s page and
                // writer_heads/tails[dst/src] — all disjoint across pairs.
                thread::scope(|s| {
                    for &(pair_dst, pair_src) in &pairs {
                        s.spawn(move || self.chain_onto(pair_dst, pair_src));
                    }
                });

                // Survivors for next level: even-indexed elements (the merge targets)
                active = active.into_iter().step_by(2).collect();
            }

            // active[0] now heads the full merged chain
            self.chain_onto(dst_id, active[0]);
        }
    }
}

// -----------------------------------------------------------------------------
// N→1  AggregateConnection
// -----------------------------------------------------------------------------

pub struct AggregateConnection {
    upstream_ids: Vec<usize>,
    downstream_id: usize,
}

impl AggregateConnection {
    pub fn new(upstream_ids: &[usize], downstream_id: usize) -> Self {
        Self { upstream_ids: upstream_ids.to_vec(), downstream_id }
    }

    pub fn bridge(&self, splice_addr: usize) {
        ShmIo::new(splice_addr).merge_into(&self.upstream_ids, self.downstream_id);
    }
}

// -----------------------------------------------------------------------------
// N→M  ShuffleConnection
// -----------------------------------------------------------------------------

pub struct ShuffleConnection {
    upstream_ids: Vec<usize>,
    downstream_ids: Vec<usize>,
    /// Maps upstream_id → downstream slot. Whole-stream granularity:
    /// all pages from one upstream go to one reducer.
    partition_fn: Box<dyn Fn(usize) -> usize + Send + Sync>,
}

impl ShuffleConnection {
    pub fn new<F>(upstream_ids: &[usize], downstream_ids: &[usize], partition_fn: F) -> Self
    where
        F: Fn(usize) -> usize + Send + Sync + 'static,
    {
        Self {
            upstream_ids: upstream_ids.to_vec(),
            downstream_ids: downstream_ids.to_vec(),
            partition_fn: Box::new(partition_fn),
        }
    }
}

impl ShuffleConnection {
    /// Group upstreams by downstream slot, then merge_into each slot.
    /// Groups are independent and can be processed concurrently.
    pub fn bridge(&self, splice_addr: usize) {
        let mut groups: Vec<Vec<usize>> = vec![Vec::new(); self.downstream_ids.len()];
        for &up in &self.upstream_ids {
            let slot = (self.partition_fn)(up);
            if slot < groups.len() {
                groups[slot].push(up);
            }
        }

        let io = ShmIo::new(splice_addr);
        thread::scope(|s| {
            for (slot, group) in groups.iter().enumerate() {
                if group.is_empty() { continue; }
                let dst_id = self.downstream_ids[slot];
                s.spawn(move || io.merge_into(group, dst_id));
            }
        });
    }
}
