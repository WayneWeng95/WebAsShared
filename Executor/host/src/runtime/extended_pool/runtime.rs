//! Process-global singleton wrapper around [`ExtendedPool`].
//!
//! The reclaimer's `alloc_page` / `free_page_chain` are free functions
//! called from dozens of sites across the `host` crate.  Threading an
//! `&mut ExtendedPool` through every caller would be a large
//! mechanical refactor for no runtime benefit — each OS process has
//! exactly one `ExtendedPool` anyway.  So we stash it in a
//! `OnceLock<Mutex<ExtendedPool>>` and expose narrow helpers that
//! handle locking for the caller.
//!
//! ## Fast-path escape hatch
//!
//! Direct-mode allocation is performance-critical: it's an atomic
//! bump plus a couple of branches today.  We must NOT hold a
//! `Mutex` on every alloc/free.  Instead:
//!
//! - `current_mode()` reads an `AtomicU8` mode flag with `Acquire`
//!   ordering.  No lock.  The reclaimer's direct path consults this
//!   and skips into the existing fast path when the flag is `Direct`.
//! - Only when the fast-path flip condition fires (bump crosses the
//!   80% threshold) does the reclaimer actually acquire the mutex to
//!   transition state.  That's a rare event, so the cost is amortized.
//! - Same for `notify_bump_advance`: it short-circuits on the atomic
//!   check and only locks when a flip is imminent.

use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{anyhow, Result};
use common::{EXTENDED_POOL_ENABLED, Page, PageId, ShmOffset, DIRECT_LIMIT};

use super::{ExtendedPool, Mode};
use super::mode::should_enter_paged;

/// The one and only extended pool for this process.
static POOL: OnceLock<Mutex<ExtendedPool>> = OnceLock::new();

/// Cached mode flag.  Reads are lock-free; writes are done under
/// `POOL.lock()` so the flag is in lock-step with `ExtendedPool::mode`.
/// `Direct = 0`, `Paged = 1`.
static MODE: AtomicU8 = AtomicU8::new(0);

const MODE_DIRECT: u8 = 0;
const MODE_PAGED: u8 = 1;

/// Whether the feature is compiled in.  All public entry points
/// short-circuit when this is `false` and the compiler eliminates
/// their bodies as dead code.  Single source of truth lives in
/// `common::EXTENDED_POOL_ENABLED`.
#[inline(always)]
pub const fn enabled() -> bool { EXTENDED_POOL_ENABLED }

/// Lock and return a mutable handle to the process-global extended pool.
/// Lazily initializes on first call.  Callers should guard with
/// [`enabled`] to avoid spinning up the singleton when the feature is
/// disabled — the public helpers in this module already do so.
pub fn lock() -> std::sync::MutexGuard<'static, ExtendedPool> {
    POOL.get_or_init(|| Mutex::new(ExtendedPool::new()))
        .lock()
        .expect("extended pool mutex poisoned")
}

/// Lock-free mode check.  Use this to decide whether to take the fast
/// direct path or dispatch to the extended pool.  Always reports
/// `Direct` when the feature flag is off.
#[inline]
pub fn current_mode() -> Mode {
    if !enabled() {
        return Mode::Direct;
    }
    match MODE.load(Ordering::Acquire) {
        MODE_PAGED => Mode::Paged,
        _          => Mode::Direct,
    }
}

/// Called from `reclaimer::alloc_page` immediately after a successful
/// direct-mode bump.  Cheap in the common case: a single atomic read
/// plus an inequality compare.  Only acquires the pool mutex on the
/// rare tick that crosses the 80% threshold.
///
/// `splice_addr` is the host-side base of the wasm32 SHM window and
/// is needed by `flip_to_paged` to anchor its `ResolutionBuffer`.
/// The reclaimer already has it in scope at the call site.
///
/// Entirely compiled out when `EXTENDED_POOL_ENABLED == false`.
#[inline]
pub fn notify_bump_advance(new_bump: ShmOffset, splice_addr: usize) {
    if !enabled() {
        return;
    }
    // Fast path: already paged.  Nothing to check.
    if MODE.load(Ordering::Acquire) == MODE_PAGED {
        return;
    }
    // Fast path: below threshold.
    if !should_enter_paged(new_bump) {
        return;
    }
    // Slow path: threshold crossed, actually flip under the lock.
    let mut pool = lock();
    // Re-check after acquiring the lock (another thread may have
    // raced us to the flip).
    if pool.mode() == Mode::Direct {
        if let Err(e) = pool.check_bump(new_bump, splice_addr) {
            eprintln!("[ExtendedPool] flip_to_paged failed: {e}");
            return;
        }
        // If the pool flipped, publish the new mode to the atomic.
        if pool.mode() == Mode::Paged {
            MODE.store(MODE_PAGED, Ordering::Release);
        }
    }
}

/// Explicitly force-flip to paged mode (used by tests).  Returns the
/// mode after the call.  Errors out when the feature flag is off.
pub fn flip_to_paged(splice_addr: usize) -> Result<Mode> {
    if !enabled() {
        return Err(anyhow::anyhow!(
            "extended_pool feature is disabled (common::EXTENDED_POOL_ENABLED = false)"
        ));
    }
    let mut pool = lock();
    pool.flip_to_paged(splice_addr)?;
    MODE.store(MODE_PAGED, Ordering::Release);
    Ok(pool.mode())
}

/// Resolve a PageId to a wasm32 `*mut Page` on the host side.
///
/// - Direct ids (`< DIRECT_LIMIT`) resolve by arithmetic against
///   `splice_addr`.  Lock-free and cheap.
/// - Paged ids go through the singleton `ExtendedPool::resolve`,
///   which locks the mutex.  If the page is not currently resident,
///   the resolve call will install it via the resolution buffer.
///
/// When the feature flag is off this always takes the arithmetic
/// branch because paged ids can never exist.
#[inline]
pub fn resolve(id: PageId, splice_addr: usize) -> Result<*mut Page> {
    if !enabled() || id < DIRECT_LIMIT {
        return Ok((splice_addr + id as usize) as *mut Page);
    }
    let mut pool = lock();
    pool.resolve(id, splice_addr)
}

/// Allocate a paged-mode page via the singleton.  Called by the
/// reclaimer as a fallback when the direct bump allocator is
/// exhausted.  Errors when the feature flag is off or when the pool
/// is still in `Direct` mode.
pub fn alloc_paged_page(rdma_bound: bool) -> Result<(PageId, *mut Page)> {
    if !enabled() {
        return Err(anyhow!(
            "extended_pool feature is disabled (common::EXTENDED_POOL_ENABLED = false)"
        ));
    }
    let mut pool = lock();
    if pool.mode() != Mode::Paged {
        return Err(anyhow!(
            "alloc_paged_page: ExtendedPool is in {:?} mode, not Paged",
            pool.mode(),
        ));
    }
    pool.alloc_page(rdma_bound)
}

/// Free a paged-mode PageId via the singleton.
pub fn free_paged_page(id: PageId) -> Result<()> {
    if !enabled() {
        return Err(anyhow!(
            "extended_pool feature is disabled (common::EXTENDED_POOL_ENABLED = false)"
        ));
    }
    debug_assert!(id >= DIRECT_LIMIT);
    let mut pool = lock();
    pool.free_page(id)
}

/// Notify that a direct-mode PageId has been freed.  Only effective
/// while the pool is in `Paged` mode — the direct slot joins the
/// resolution buffer's free pool instead of the direct freelist.
/// Safe to call unconditionally; the singleton handles the mode check.
pub fn notify_direct_free(id: PageId) {
    if !enabled() {
        return;
    }
    if MODE.load(Ordering::Acquire) != MODE_PAGED {
        return;
    }
    let mut pool = lock();
    pool.on_direct_free(id);
}

/// Test-only: reset the singleton back to a fresh `Direct` state.
/// Because `OnceLock` cannot be re-initialized, this reaches through
/// the mutex and overwrites the inner value.  Not exposed in release
/// builds.
#[cfg(test)]
pub fn reset_for_test() {
    if let Some(mutex) = POOL.get() {
        let mut guard = mutex.lock().unwrap();
        *guard = ExtendedPool::new();
    }
    MODE.store(MODE_DIRECT, Ordering::Release);
}
