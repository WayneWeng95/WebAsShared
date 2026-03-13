extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;

use common::*;

pub(crate) const SHM_BASE: usize = 0x8000_0000;
pub(crate) static mut LOCAL_CAPACITY: u32 = 36 * MIB;
pub(crate) static mut ATOMIC_INDEX_CACHE: Option<BTreeMap<String, u32>> = None;

/// Seek position specifier, mirroring POSIX `lseek(2)` whence values.
/// The caller owns the current position and passes it back on each call.
pub enum SeekFrom {
    /// Move to an absolute byte offset from the start of the data.
    Start(u32),
    /// Move relative to the current cursor position (signed delta).
    Current(i32),
    /// Move relative to the end of the data (signed delta; use negative to seek backward).
    End(i32),
}

pub struct ShmApi;

mod page_allocator;
mod atomic_arena;
mod fan_out;
mod log_arena;
mod output;
mod shared_area;
mod stream_area;
