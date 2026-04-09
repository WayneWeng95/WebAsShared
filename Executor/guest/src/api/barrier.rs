use super::ShmApi;

extern "C" {
    fn host_barrier_wait(barrier_id: u32, party_count: u32);
}

impl ShmApi {
    /// Block until all `party_count` callers have reached this barrier.
    ///
    /// All nodes in the same execution wave that share a barrier ID must call
    /// this with the same `party_count`.  The call is backed by a Linux futex,
    /// so waiting processes sleep with zero CPU cost until the last party
    /// arrives and wakes them.
    ///
    /// A single workload function may call this multiple times with different
    /// `barrier_id` values to create multiple synchronization points (e.g.
    /// produce → barrier 0 → exchange → barrier 1 → consume).
    ///
    /// # Arguments
    /// * `barrier_id`   — index into the Superblock barrier array (0..63).
    /// * `party_count`  — number of processes that must arrive before any
    ///                    are released.
    ///
    /// # Panics
    /// Panics (trap) if `barrier_id >= BARRIER_COUNT` (bounds checked on the
    /// host side via the Superblock array access).
    pub fn barrier_wait(barrier_id: u32, party_count: u32) {
        unsafe { host_barrier_wait(barrier_id, party_count) }
    }
}
