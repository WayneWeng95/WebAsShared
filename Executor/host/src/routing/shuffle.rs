// Host-side N→M shuffle via zero-copy page chain splicing.
//
// chain_onto is O(1): writer_tails[id] already holds the tail page offset,
// so no page-walking is needed. For N > PARALLEL_THRESHOLD upstreams per
// downstream slot, a parallel tree merge reduces the sequential dependency
// chain from O(N) to O(log N) levels, each level run concurrently.
//
// 1→1 connections: use HostStream::bridge directly (two atomic stores).
// N→1 connections: use AggregateConnection (aggregate.rs).
// N→M broadcast:   use BroadcastConnection (broadcast.rs).

use std::thread;
use super::chain_splicer::ChainSplicer;
use crate::policy::ShufflePolicy;

// -----------------------------------------------------------------------------
// N→M  ShuffleConnection
// -----------------------------------------------------------------------------

pub struct ShuffleConnection {
    upstream_ids: Vec<usize>,
    downstream_ids: Vec<usize>,
    /// Partitioning policy: maps each upstream_id to a downstream slot index.
    /// Whole-stream granularity — all pages from one upstream go to one reducer.
    policy: Box<dyn ShufflePolicy>,
}

impl ShuffleConnection {
    /// Construct a shuffle connection with any `ShufflePolicy` implementation.
    /// `policy.partition(upstream_id, num_downstream)` is called once per upstream
    /// during `bridge` to determine which downstream slot it routes to.
    pub fn new<P: ShufflePolicy + 'static>(
        upstream_ids: &[usize],
        downstream_ids: &[usize],
        policy: P,
    ) -> Self {
        Self {
            upstream_ids: upstream_ids.to_vec(),
            downstream_ids: downstream_ids.to_vec(),
            policy: Box::new(policy),
        }
    }

    /// Group upstreams by downstream slot (via the policy), then merge_into each slot.
    /// Groups are independent and can be processed concurrently.
    pub fn bridge(&self, splice_addr: usize) {
        let num_downstream = self.downstream_ids.len();
        let mut groups: Vec<Vec<usize>> = vec![Vec::new(); num_downstream];
        for &up in &self.upstream_ids {
            let slot = self.policy.partition(up, num_downstream);
            if slot < groups.len() {
                groups[slot].push(up);
            }
        }

        let io = ChainSplicer::new(splice_addr);
        thread::scope(|s| {
            for (slot, group) in groups.iter().enumerate() {
                if group.is_empty() { continue; }
                let dst_id = self.downstream_ids[slot];
                s.spawn(move || io.merge_into(group, dst_id));
            }
        });
    }
}
