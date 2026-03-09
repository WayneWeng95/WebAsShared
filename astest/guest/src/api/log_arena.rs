use core::sync::atomic::Ordering;
use common::*;
use super::{ShmApi, SHM_BASE};

impl ShmApi {
    /// Appends a debug message to the shared log arena.
    /// Reserves space atomically; silently drops the message if the arena is full.
    pub fn append_log(msg: &str) {
        let sb = Self::superblock();
        let bytes = msg.as_bytes();
        let len = bytes.len() as u32;

        let offset = sb.log_offset.fetch_add(len, Ordering::Relaxed);
        if offset + len <= LOG_ARENA_SIZE {
            let dest = (SHM_BASE + LOG_ARENA_OFFSET as usize + offset as usize) as *mut u8;
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len()); }
        }
    }
}
