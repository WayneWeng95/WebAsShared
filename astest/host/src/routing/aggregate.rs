// N→1 aggregate connection: merges N upstream page chains into one downstream slot.

use super::chain_splicer::ChainSplicer;

/// Merges every upstream stream into a single downstream slot.
///
/// All pages from all upstream slots are spliced (zero-copy) onto the end of
/// the downstream slot's page chain.  For N ≤ PARALLEL_THRESHOLD upstreams
/// the merge runs sequentially; beyond that a parallel tree merge is used.
pub struct AggregateConnection {
    upstream_ids: Vec<usize>,
    downstream_id: usize,
}

impl AggregateConnection {
    pub fn new(upstream_ids: &[usize], downstream_id: usize) -> Self {
        Self { upstream_ids: upstream_ids.to_vec(), downstream_id }
    }

    pub fn bridge(&self, splice_addr: usize) {
        ChainSplicer::new(splice_addr).merge_into(&self.upstream_ids, self.downstream_id);
    }
}
