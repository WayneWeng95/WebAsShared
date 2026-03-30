// Host-side shuffle partitioning policies.
//
// A `ShufflePolicy` maps an upstream stream ID to a downstream slot index in
// [0, num_downstream).  Implementations are passed to `make_partition_fn` to
// produce the closure expected by `ShuffleConnection::new`.
//
// Usage:
//   let conn = ShuffleConnection::new(
//       &upstream_ids,
//       &downstream_ids,
//       make_partition_fn(ModuloPartition, downstream_ids.len()),
//   );

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// -----------------------------------------------------------------------------
// Trait
// -----------------------------------------------------------------------------

/// Determines which downstream slot an upstream stream is routed to.
///
/// `upstream_id`   — the source stream slot index.
/// `num_downstream` — total number of downstream slots (partition target count).
///
/// The returned value must be in `[0, num_downstream)`.
pub trait ShufflePolicy: Send + Sync {
    fn partition(&self, upstream_id: usize, num_downstream: usize) -> usize;
}

// -----------------------------------------------------------------------------
// Helper: convert any ShufflePolicy into the closure ShuffleConnection expects
// -----------------------------------------------------------------------------

/// Wraps a `ShufflePolicy` into a `Fn(usize) -> usize` closure suitable for
/// `ShuffleConnection::new`.  `num_downstream` is baked in at call time so the
/// returned closure only takes the upstream id, matching the required signature.
///
/// ```ignore
/// ShuffleConnection::new(
///     &[0, 1, 2, 3],
///     &[4, 5],
///     make_partition_fn(ModuloPartition, 2),
/// ).bridge(splice_addr);
/// ```
pub fn make_partition_fn<P>(policy: P, num_downstream: usize)
    -> impl Fn(usize) -> usize + Send + Sync + 'static
where
    P: ShufflePolicy + 'static,
{
    move |upstream_id| policy.partition(upstream_id, num_downstream)
}

// -----------------------------------------------------------------------------
// ModuloPartition
// -----------------------------------------------------------------------------

/// Routes upstream `id` to slot `id % num_downstream`.
///
/// Simple and deterministic: upstream 0 → slot 0, upstream 1 → slot 1, …
/// wrapping around once all slots are covered.  Requires no state.
pub struct ModuloPartition;

impl ShufflePolicy for ModuloPartition {
    fn partition(&self, upstream_id: usize, num_downstream: usize) -> usize {
        upstream_id % num_downstream
    }
}

// -----------------------------------------------------------------------------
// RoundRobinPartition
// -----------------------------------------------------------------------------

/// Assigns each successive upstream call to the next slot in a cycle
/// (0, 1, 2, …, num_downstream-1, 0, 1, …).
///
/// Unlike `ModuloPartition`, the assignment is based on call order rather than
/// the upstream ID value, so IDs that share the same residue are spread across
/// different slots.  Thread-safe via an internal `AtomicUsize` counter.
///
/// # Example
/// With 3 upstreams [5, 5, 5] and 2 downstream slots:
///   call 0 → slot 0
///   call 1 → slot 1
///   call 2 → slot 0
pub struct RoundRobinPartition {
    counter: Arc<AtomicUsize>,
}

impl RoundRobinPartition {
    pub fn new() -> Self {
        Self { counter: Arc::new(AtomicUsize::new(0)) }
    }
}

impl ShufflePolicy for RoundRobinPartition {
    fn partition(&self, _upstream_id: usize, num_downstream: usize) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed) % num_downstream
    }
}

// -----------------------------------------------------------------------------
// FixedMapPartition
// -----------------------------------------------------------------------------

/// Routes each upstream ID to a pre-configured downstream slot via an explicit
/// lookup table.  Upstreams not present in the map fall back to `default_slot`.
///
/// Useful when the mapping is irregular (e.g. upstream 3 → slot 0, upstream 7
/// → slot 1) and cannot be expressed by a simple formula.
///
/// # Example
/// ```ignore
/// let policy = FixedMapPartition::new(
///     [(0, 1), (1, 0), (2, 1)].into_iter().collect(),
///     0,   // fallback slot
/// );
/// ```
pub struct FixedMapPartition {
    map: HashMap<usize, usize>,
    default_slot: usize,
}

impl FixedMapPartition {
    pub fn new(map: HashMap<usize, usize>, default_slot: usize) -> Self {
        Self { map, default_slot }
    }
}

impl ShufflePolicy for FixedMapPartition {
    fn partition(&self, upstream_id: usize, _num_downstream: usize) -> usize {
        *self.map.get(&upstream_id).unwrap_or(&self.default_slot)
    }
}
