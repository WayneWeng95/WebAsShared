use core::sync::atomic::{AtomicU64, Ordering};
use alloc::collections::BTreeMap;
use alloc::string::String;
use common::*;
use super::{ShmApi, SHM_BASE};

extern "C" {
    fn host_resolve_atomic(ptr: u32, len: u32) -> u32;
}

impl ShmApi {
    /// Returns a reference to the `AtomicU64` at the given index in the shared atomic arena.
    fn get_atomic_by_index(index: usize) -> &'static AtomicU64 {
        let base = SHM_BASE + ATOMIC_ARENA_OFFSET as usize;
        unsafe { &*((base as *const AtomicU64).add(index)) }
    }

    /// Returns a reference to the `AtomicU64` at the given registry index.
    pub fn get_atomic(index: usize) -> &'static AtomicU64 { Self::get_atomic_by_index(index) }

    /// Looks up a named atomic variable, registering it in the host Registry on first access.
    /// The resolved index is cached locally to avoid repeated `host_resolve_atomic` calls.
    pub fn get_named_atomic(name: &str) -> &'static AtomicU64 {
        unsafe {
            if super::ATOMIC_INDEX_CACHE.is_none() { super::ATOMIC_INDEX_CACHE = Some(BTreeMap::new()); }
            let cache = super::ATOMIC_INDEX_CACHE.as_mut().unwrap();
            let index = if let Some(&idx) = cache.get(name) { idx } else {
                let idx = host_resolve_atomic(name.as_ptr() as u32, name.len() as u32);
                cache.insert(String::from(name), idx);
                idx
            };
            Self::get_atomic_by_index(index as usize)
        }
    }
}
