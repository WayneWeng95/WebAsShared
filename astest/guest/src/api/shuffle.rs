// use for a easy shuffle setup
use alloc::vec::Vec;
use super::ShmApi;

// -----------------------------------------------------------------------------
// Design note — host vs guest responsibility
// -----------------------------------------------------------------------------
// True N→M shuffle (cross-worker routing) belongs on the HOST side.
// The host has a global view of all shared-memory streams and can move data
// between any two workers without occupying a guest WASM sandbox as a router.
// Doing N→M routing inside a guest sandbox creates a serial bottleneck:
// one worker must be scheduled purely to read N upstreams and fan-out to M
// downstreams, which wastes a slot and adds an extra scheduling round-trip.
//
// What the guest CAN do efficiently:
//   - N→1 aggregation (AggregatePipe): a reduce worker pulls from all its
//     map-layer predecessors into itself — no cross-worker writes needed.
//   - The guest can also HINT the host about the desired partition key by
//     writing routing metadata to its stream header (future extension).
// -----------------------------------------------------------------------------

/// N→1 aggregation: a single worker collects the latest output from every
/// worker in its upstream layer, merges them, and writes to its own stream.
///
/// ```
///   upstream layer     AggregatePipe       this worker
///   worker 0 ──┐                           ┌─────────┐
///   worker 1 ──┼──► collect_all ──► merge ─► self_id │
///   worker 2 ──┤                           └─────────┘
///   worker 3 ──┘
/// ```
///
/// Usage:
/// ```ignore
/// let agg = AggregatePipe::from_range(0, 4, self_id);
///
/// // Merge all upstream payloads into own stream
/// agg.reduce(|inputs| {
///     inputs.into_iter().flat_map(|(_, data)| data).collect()
/// });
///
/// // Or inspect each input before merging
/// for (upstream_id, data) in agg.collect_all() { ... }
/// ```
pub struct AggregatePipe {
    upstream_ids: Vec<u32>,
    self_id: u32,
}

impl AggregatePipe {
    /// Explicit upstream id list.
    pub fn new(upstream_ids: &[u32], self_id: u32) -> Self {
        Self { upstream_ids: upstream_ids.to_vec(), self_id }
    }

    /// Contiguous upstream range `[first, first+count)`.
    pub fn from_range(first: u32, count: u32, self_id: u32) -> Self {
        Self { upstream_ids: (first..first + count).collect(), self_id }
    }

    /// Poll every upstream worker; returns `(upstream_id, payload)` for each
    /// worker that has data. Workers with no output yet are silently skipped.
    pub fn collect_all(&self) -> Vec<(u32, Vec<u8>)> {
        let mut results = Vec::new();
        for &id in &self.upstream_ids {
            if let Some(data) = ShmApi::read_latest_stream_data(id) {
                results.push((id, data));
            }
        }
        results
    }

    /// Write `data` to this worker's own output stream.
    pub fn write_output(&self, data: &[u8]) {
        ShmApi::append_stream_data(self.self_id, data);
    }

    /// Collect all upstream inputs, pass through `merge_fn`, write result to
    /// own stream. Returns `true` if at least one upstream had data.
    pub fn reduce<F: Fn(Vec<(u32, Vec<u8>)>) -> Vec<u8>>(&self, merge_fn: F) -> bool {
        let inputs = self.collect_all();
        if inputs.is_empty() { return false; }
        self.write_output(&merge_fn(inputs));
        true
    }
}

// -----------------------------------------------------------------------------
// N→M ShufflePipe — guest-side approximation
// -----------------------------------------------------------------------------
// This is provided for completeness and testing, but for production workloads
// the host should drive the shuffle so no guest slot is wasted as a router.
// Use this only when a dedicated coordinator WASM worker is acceptable.
// -----------------------------------------------------------------------------

/// N→M shuffle: one coordinator worker reads from N upstreams and routes each
/// record to one of M downstream workers via a partition function.
///
/// NOTE: Prefer host-driven shuffle for production. See design note above.
///
/// ```
///   upstream layer        ShufflePipe          downstream layer
///   worker 0 ──┐                               ┌── worker 4
///   worker 1 ──┼──► collect_all ──► route ─────┼── worker 5
///   worker 2 ──┤          (partition_fn)       ├── worker 6
///   worker 3 ──┘                               └── worker 7
/// ```
pub struct ShufflePipe {
    upstream_ids: Vec<u32>,
    downstream_ids: Vec<u32>,
}

impl ShufflePipe {
    /// Explicit upstream and downstream id lists.
    pub fn new(upstream_ids: &[u32], downstream_ids: &[u32]) -> Self {
        Self {
            upstream_ids: upstream_ids.to_vec(),
            downstream_ids: downstream_ids.to_vec(),
        }
    }

    /// Contiguous ranges: upstream `[up_first, up_first+up_count)`,
    /// downstream `[down_first, down_first+down_count)`.
    pub fn from_ranges(up_first: u32, up_count: u32, down_first: u32, down_count: u32) -> Self {
        Self {
            upstream_ids:   (up_first..up_first + up_count).collect(),
            downstream_ids: (down_first..down_first + down_count).collect(),
        }
    }

    /// Poll every upstream worker; returns `(upstream_id, payload)` for each
    /// worker that has data.
    pub fn collect_all(&self) -> Vec<(u32, Vec<u8>)> {
        let mut results = Vec::new();
        for &id in &self.upstream_ids {
            if let Some(data) = ShmApi::read_latest_stream_data(id) {
                results.push((id, data));
            }
        }
        results
    }

    /// Write `payload` to the downstream worker at slot index. No-op if out of range.
    pub fn route_to(&self, slot: usize, payload: &[u8]) {
        if let Some(&dst_id) = self.downstream_ids.get(slot) {
            ShmApi::append_stream_data(dst_id, payload);
        }
    }

    /// Collect all upstream inputs and route each to a downstream slot.
    /// `partition_fn(upstream_id, payload)` returns the downstream slot index.
    /// Returns the number of records routed.
    pub fn shuffle<F: Fn(u32, &[u8]) -> usize>(&self, partition_fn: F) -> usize {
        let inputs = self.collect_all();
        let count = inputs.len();
        for (upstream_id, data) in &inputs {
            let slot = partition_fn(*upstream_id, data);
            self.route_to(slot, data);
        }
        count
    }
}
