//! Allocation mode state machine.
//!
//! Two-state FSM:
//!
//! ```text
//!                 bump >= 80% of DIRECT_LIMIT
//!        Direct ─────────────────────────────▶ Paged
//!           ▲                                    │
//!           │  global_live == 0                  │
//!           │  && bump < 50% of DIRECT_LIMIT     │
//!           └────────────────────────────────────┘
//! ```
//!
//! `Direct → Paged` is triggered by `ExtendedPool::on_bump_advance`
//! from the reclaimer's alloc path.  `Paged → Direct` is triggered
//! inside `ExtendedPool::free_page` when the last paged-mode PageId
//! is released and the direct bump fill has fallen below the exit
//! threshold.
//!
//! The exit condition has two parts: `global_live == 0` is the
//! correctness gate (no paged PageIds exist anywhere, so nothing can
//! reference the resolution buffer), while the 50% fill is hysteresis
//! to avoid flapping near the 80% entry threshold.

use std::sync::OnceLock;

use common::{DIRECT_LIMIT, PAGED_MODE_ENTER_DEN, PAGED_MODE_ENTER_NUM,
             PAGED_MODE_EXIT_DEN, PAGED_MODE_EXIT_NUM, ShmOffset};

/// Current allocation mode for one subprocess's page pool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Normal allocation inside the 2 GiB wasm32 window.  PageIds are
    /// `< DIRECT_LIMIT` and equal their own wasm32 byte offset.
    Direct,
    /// Overflow mode.  New PageIds come from the host-side global pool
    /// and carry values `>= DIRECT_LIMIT`.  Any data in already-allocated
    /// direct pages stays in place and is still addressed by its
    /// direct PageId.
    Paged,
}

/// Parse the optional `WEBS_FORCE_FLIP_AT` environment variable once.
/// When set, its value (decimal or `0x`-prefixed hex) overrides the
/// 80% production threshold for the `Direct -> Paged` flip.  Used by
/// tests and integration smoke tests so the flip can be exercised
/// without a 2 GiB workload.  Parsed exactly once at first call.
fn debug_enter_threshold() -> Option<u64> {
    static CACHE: OnceLock<Option<u64>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let raw = std::env::var("WEBS_FORCE_FLIP_AT").ok()?;
        if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16).ok()
        } else {
            raw.parse::<u64>().ok()
        }
    })
}

/// Should we flip `Direct -> Paged` given the current bump offset?
///
/// Honors the `WEBS_FORCE_FLIP_AT` environment override when set,
/// otherwise uses the 80% production threshold.
#[inline]
pub fn should_enter_paged(bump_offset: ShmOffset) -> bool {
    if let Some(thresh) = debug_enter_threshold() {
        return (bump_offset as u64) >= thresh;
    }
    // Compare `bump / DIRECT_LIMIT >= 8/10` without floats.
    //   bump * 10 >= DIRECT_LIMIT * 8
    // Widening to u64 to avoid overflow for values near DIRECT_LIMIT.
    (bump_offset as u64) * PAGED_MODE_ENTER_DEN
        >= DIRECT_LIMIT * PAGED_MODE_ENTER_NUM
}

/// Should we flip `Paged -> Direct` given the current bump offset and
/// live-paged count?
#[inline]
pub fn should_exit_paged(bump_offset: ShmOffset, global_live: u64) -> bool {
    if global_live != 0 {
        return false;
    }
    (bump_offset as u64) * PAGED_MODE_EXIT_DEN
        < DIRECT_LIMIT * PAGED_MODE_EXIT_NUM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_at_80_percent() {
        assert!(!should_enter_paged(0));
        assert!(!should_enter_paged((DIRECT_LIMIT as ShmOffset) / 2));
        // 80% of 2 GiB = 1.6 GiB
        let eighty = ((DIRECT_LIMIT * 8 / 10) as ShmOffset) + 1;
        assert!(should_enter_paged(eighty));
    }

    #[test]
    fn exit_requires_zero_live_and_below_50_percent() {
        let half = (DIRECT_LIMIT as ShmOffset) / 2;
        // 50% fill but live > 0: stay paged.
        assert!(!should_exit_paged(half.saturating_sub(1), 1));
        // 50% fill, no live: flip back.
        assert!(should_exit_paged(half.saturating_sub(1), 0));
        // Just above 50% with no live: stay paged (hysteresis).
        assert!(!should_exit_paged(half + 1, 0));
    }
}
