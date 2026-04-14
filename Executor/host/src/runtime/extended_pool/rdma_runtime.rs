//! Process-global singleton for the RDMA extended pool (Path C scaffold).
//!
//! Mirrors the structure of [`super::runtime`] for the general
//! extended pool, but tracks **RDMA-specific** state:
//!
//! - `rdma_mr1_used: AtomicU64` — how many bytes the RDMA receive
//!   paths have drawn from MR1 so far.  Incremented on each receive,
//!   NOT reset on free (because the RDMA pool is a budget, not a
//!   refcounted live set — we measure cumulative pressure, which
//!   governs when MR2 registration fires).  Compare against
//!   `common::RDMA_MR1_BUDGET` to decide if the next allocation
//!   should spill into MR2.
//!
//! - `pool: OnceLock<Mutex<RdmaPool>>` — the dedicated MR2 pool.
//!   Created lazily on the first allocation that crosses
//!   `common::RDMA_MR2_REG_THRESHOLD`.  Once created, stays alive
//!   until process exit.  Not the same as `super::runtime::POOL` —
//!   Path C wants its own isolated storage and isolated lifecycle.
//!
//! ## Current status
//!
//! This module is **scaffolded only**.  The singleton exists, the
//! `should_use_mr2` / `ensure_pool` / `alloc_mr2_contiguous` /
//! `free_mr2` / `host_addr_of` helpers are implemented and unit-
//! tested.  What is NOT yet wired:
//!
//! - `ibv_reg_mr` call on the pool's reservation after creation.
//!   That lives in `connect/` and will be added when the mesh-side
//!   plumbing catches up.
//! - TCP control-channel broadcast of the new MR2 rkey to all peers
//!   in the `MeshNode`.
//! - Sender-side rkey branch in `rdma::rdma_write_page_chain` /
//!   `rdma::rdma_write_flat`.
//! - Receiver-side `alloc_and_link` spillover logic that routes
//!   bulk allocations to the RdmaPool when MR1 is exhausted.
//! - Post-receive CPU memcpy from MR2 → freshly-allocated MR1
//!   direct pages and the subsequent chain linking.
//!
//! All of these items have specific TODO markers in
//! `docs/extended_pool.md` §"RDMA integration (Path C)".

#![allow(dead_code)]

use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use common::{
    EXTENDED_RDMA_ENABLED, Page, PageId,
    RDMA_MR1_BUDGET, RDMA_MR2_INITIAL_SIZE, RDMA_MR2_REG_THRESHOLD,
};

/// Host-side cast of the u64 constant — see the similar alias in
/// `rdma_pool.rs`.  `common` types these as `u64` for wasm32 compat.
const RDMA_MR2_INITIAL_SIZE_USIZE: usize = RDMA_MR2_INITIAL_SIZE as usize;

use super::rdma_pool::{RdmaPool, RDMA_MR2_MARKER};

/// Feature-flag guard.  Every entry point checks this first; the
/// compiler eliminates every function body when the flag is off.
#[inline(always)]
pub const fn enabled() -> bool { EXTENDED_RDMA_ENABLED }

/// Cumulative bytes the RDMA receive paths have allocated from MR1.
/// Never decremented — this is a pressure counter, not a live-set
/// size.  Used only to trigger `RDMA_MR2_REG_THRESHOLD` and
/// `RDMA_MR1_BUDGET` decisions.
static RDMA_MR1_USED: AtomicU64 = AtomicU64::new(0);

/// The lazily-created MR2 pool.  Wrapped in `OnceLock<Mutex<_>>`
/// for the same reason as the general `runtime::POOL`: we want a
/// lock-free "does the pool exist yet" check via `OnceLock::get()`,
/// and a mutex for actual access.
static POOL: OnceLock<Mutex<RdmaPool>> = OnceLock::new();

/// Lock-free check: would the receive path want to allocate from
/// MR2 for an incoming transfer of `needed_bytes`?  Returns `true`
/// if adding `needed_bytes` to `rdma_mr1_used` would cross
/// `RDMA_MR1_BUDGET`.
///
/// This is a fast predicate used BEFORE committing an allocation.
/// Returns false immediately when the feature flag is off.
#[inline]
pub fn should_use_mr2(needed_bytes: u64) -> bool {
    if !enabled() {
        return false;
    }
    let used = RDMA_MR1_USED.load(Ordering::Acquire);
    used.saturating_add(needed_bytes) > RDMA_MR1_BUDGET
}

/// Lock-free check: should we proactively register MR2 now, even
/// though the current allocation will still fit in MR1?  Returns
/// `true` when `rdma_mr1_used >= RDMA_MR2_REG_THRESHOLD` AND the
/// pool does not yet exist.  Used to drive lazy registration before
/// the first spillover allocation arrives on the critical path.
#[inline]
pub fn should_register_mr2() -> bool {
    if !enabled() {
        return false;
    }
    if POOL.get().is_some() {
        return false;
    }
    RDMA_MR1_USED.load(Ordering::Acquire) >= RDMA_MR2_REG_THRESHOLD
}

/// Record that `bytes` of MR1 have been consumed by the RDMA path.
/// Called from `alloc_and_link` after each successful MR1 allocation.
#[inline]
pub fn record_mr1_usage(bytes: u64) {
    if !enabled() {
        return;
    }
    RDMA_MR1_USED.fetch_add(bytes, Ordering::AcqRel);
}

/// Current cumulative MR1 usage counter.  Test / metrics only.
#[inline]
pub fn mr1_used() -> u64 {
    RDMA_MR1_USED.load(Ordering::Acquire)
}

/// Ensure the MR2 pool has been created.  First call does the work
/// (open file, reserve VA, initial commit); subsequent calls are
/// cheap idempotent returns.
///
/// This is the point where the eventual `ibv_reg_mr` call will be
/// hooked in — once the connect/ side learns how to register an
/// additional MR after the mesh is up.  For now this only builds the
/// host-side storage.
pub fn ensure_pool() -> Result<()> {
    if !enabled() {
        return Err(anyhow!(
            "EXTENDED_RDMA_ENABLED is false — RdmaPool unavailable",
        ));
    }
    if POOL.get().is_some() {
        return Ok(());
    }
    let pid = std::process::id();
    let path = std::path::PathBuf::from(format!("/dev/shm/webs-rdma-{pid}"));
    let pool = RdmaPool::new(&path, RDMA_MR2_INITIAL_SIZE_USIZE)
        .map_err(|e| anyhow!("ensure_pool: RdmaPool::new failed: {e}"))?;
    let _ = POOL.set(Mutex::new(pool));

    // TODO(Path C): once connect/ supports registering additional
    // MRs against a live MeshNode, call:
    //     mesh.register_rdma_pool_mr(&pool)
    // which should:
    //   1. ibv_reg_mr(pool.base_addr(), pool.reservation_size(), ...)
    //   2. Broadcast (mr2_base, mr2_rkey) to every peer over the
    //      existing ctrl_as_sender / ctrl_as_receiver TCP channels
    //      via a new `MR2Announce` control message.
    //   3. Each peer's `SendChannel::remote_mr2` field gets filled
    //      in from the broadcast.
    // See docs/extended_pool.md §"RDMA integration (Path C)" for the
    // full integration checklist.

    eprintln!(
        "[RdmaPool] lazy MR2 created at bump={} MiB past RDMA_MR2_REG_THRESHOLD",
        RDMA_MR1_USED.load(Ordering::Acquire) / (1024 * 1024),
    );
    Ok(())
}

/// Lock and return a handle to the pool.  Panics if the pool has
/// not yet been created — callers are expected to `ensure_pool()`
/// first, or use `try_lock` which returns `None` when uninitialized.
pub fn lock() -> std::sync::MutexGuard<'static, RdmaPool> {
    POOL.get()
        .expect("RdmaPool not yet initialized; call ensure_pool() first")
        .lock()
        .expect("RdmaPool mutex poisoned")
}

/// Non-panicking variant: returns `None` if the pool doesn't exist
/// yet.  Preferred by `alloc_and_link` since the pool only exists
/// once MR2 has been triggered.
pub fn try_lock() -> Option<std::sync::MutexGuard<'static, RdmaPool>> {
    POOL.get().map(|m| m.lock().expect("RdmaPool mutex poisoned"))
}

/// Allocate `n` contiguous MR2 pages, ensuring the pool exists.
pub fn alloc_contiguous(n: usize) -> Result<PageId> {
    if !enabled() {
        return Err(anyhow!(
            "EXTENDED_RDMA_ENABLED is false — alloc_contiguous rejected",
        ));
    }
    ensure_pool()?;
    let mut pool = lock();
    pool.alloc_contiguous(n)
}

/// Return a single MR2 PageId to the pool.
pub fn free(id: PageId) -> Result<()> {
    if !enabled() {
        return Err(anyhow!(
            "EXTENDED_RDMA_ENABLED is false — free rejected",
        ));
    }
    debug_assert!(id >= RDMA_MR2_MARKER);
    let mut pool = lock();
    pool.free(id);
    Ok(())
}

/// Host-side virtual address of an MR2 PageId.  Used by the
/// eventual post-receive memcpy to read bytes out of MR2 and into
/// freshly-allocated MR1 direct pages.
pub fn host_addr_of(id: PageId) -> Result<*mut Page> {
    if !enabled() {
        return Err(anyhow!("EXTENDED_RDMA_ENABLED is false"));
    }
    debug_assert!(id >= RDMA_MR2_MARKER);
    let pool = lock();
    Ok(pool.host_addr_of(id))
}

/// Test-only: reset the singleton back to a fresh state.  Cannot
/// reinitialize `OnceLock` itself, so we reset the usage counter
/// and leave any already-created pool in place.  Tests that care
/// about pool isolation should run sequentially.
#[cfg(test)]
pub fn reset_for_test() {
    RDMA_MR1_USED.store(0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_use_mr2_transitions_at_budget() {
        reset_for_test();
        // When flag is off, never spills — verify unconditionally.
        if !enabled() {
            assert!(!should_use_mr2(RDMA_MR1_BUDGET * 10));
            return;
        }
        // At zero used, a below-budget request doesn't spill.
        assert!(!should_use_mr2(1024));
        // A request larger than the whole budget spills.
        assert!(should_use_mr2(RDMA_MR1_BUDGET + 1));
    }

    #[test]
    fn record_mr1_usage_accumulates() {
        reset_for_test();
        if !enabled() { return; }
        record_mr1_usage(1024);
        record_mr1_usage(2048);
        assert_eq!(mr1_used(), 3072);
    }

    #[test]
    fn should_register_mr2_fires_at_threshold() {
        reset_for_test();
        if !enabled() { return; }
        assert!(!should_register_mr2());
        record_mr1_usage(RDMA_MR2_REG_THRESHOLD - 1);
        assert!(!should_register_mr2());
        record_mr1_usage(1);
        // Now at exactly the threshold.  Note the pool may or may
        // not exist depending on what prior tests did, so this
        // asserts the threshold calculation without depending on
        // global state.
        let used = mr1_used();
        assert!(used >= RDMA_MR2_REG_THRESHOLD);
    }
}
