use crate::policy::{ConsumptionPolicy, ConsumptionResult, HostNode};
use crate::shm::TARGET_OFFSET;
use crate::worker::WorkerState;
use std::sync::atomic::{AtomicU32, Ordering};
use wasmtime::{Memory, Store};

pub struct BucketOrganizer<'a> {
    base_ptr: *mut u8,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> BucketOrganizer<'a> {
    pub fn new(store: &mut Store<WorkerState>, memory: &Memory) -> Self {
        let base_ptr = unsafe { memory.data_ptr(store).add(TARGET_OFFSET) };
        Self {
            base_ptr,
            _marker: std::marker::PhantomData,
        }
    }

    /// Core: scan all buckets, detach lists, run policy, then tear down
    pub unsafe fn consume_all_buckets<P: ConsumptionPolicy>(&self, policy: P) {
        let sb_ptr = self.base_ptr;
        let map_base_atomic = sb_ptr.add(24) as *const AtomicU32;
        let map_base_offset = (*map_base_atomic).load(Ordering::Relaxed);
        if map_base_offset == 0 {
            return;
        }

        for i in 0..1024 {
            let bucket_ptr =
                self.base_ptr.add(map_base_offset as usize).add(i * 4) as *const AtomicU32;
            let list_head = (*bucket_ptr).swap(0, Ordering::SeqCst);

            if list_head != 0 {
                self.process_detached_list(list_head, i, &policy);

                // After processing, reclaim memory
                self.recycle_chain(list_head);
            }
        }
    }

    // Process a detached list
    unsafe fn process_detached_list<P: ConsumptionPolicy>(
        &self,
        head_offset: u32,
        bucket_idx: usize,
        policy: &P,
    ) {
        let mut nodes = Vec::new();
        let mut current_offset = head_offset;

        // 1. Traverse and extract data (copy out to host heap)
        while current_offset != 0 {
            let node_ptr = self.base_ptr.add(current_offset as usize);

            // Read header
            let writer_id = *(node_ptr.add(4) as *const u32);
            let data_len = *(node_ptr.add(8) as *const u32);
            let next_offset = (*(node_ptr as *const AtomicU32)).load(Ordering::Relaxed);

            // Read payload
            let payload_ptr = node_ptr.add(12);
            let payload = std::slice::from_raw_parts(payload_ptr, data_len as usize).to_vec();

            nodes.push(HostNode {
                offset: current_offset,
                writer_id,
                data_len,
                payload,
            });

            current_offset = next_offset;
        }

        // 2. Run policy
        let result = policy.process(&nodes);

        // 3. Print result (or trigger other logic here, e.g. write to DB)
        match result {
            ConsumptionResult::Winner(id, content) => {
                println!(
                    "[Manager] Bucket #{} Consumed. Winner: Writer {} -> \"{}\" (Pool size: {})",
                    bucket_idx,
                    id,
                    content,
                    nodes.len()
                );
            }
            ConsumptionResult::None => {}
        }
    }

    unsafe fn recycle_chain(&self, list_head: u32) {
        if list_head == 0 {
            return;
        }

        // Walk the list and push each node onto the free list one by one.
        // A simple per-node push gives a safe Treiber stack; avoids race conditions
        // from splicing the whole chain at once.

        let mut current = list_head;
        while current != 0 {
            let node_ptr = self.base_ptr.add(current as usize);
            let next_ptr = &*(node_ptr as *const AtomicU32); // First field of page is next

            // Save next node offset before we modify current node's next
            let next_node = next_ptr.load(Ordering::Relaxed);

            // Push onto free list
            self.push_to_free_list(current);

            current = next_node;
        }
    }

    unsafe fn push_to_free_list(&self, page_offset: u32) {
        let sb_ptr = self.base_ptr;
        // Free list head is at superblock offset 28
        let free_list_head_ptr = sb_ptr.add(28) as *const AtomicU32;
        let free_list = &*free_list_head_ptr;

        let page_ptr = self.base_ptr.add(page_offset as usize);
        let page_next_atomic = &*(page_ptr as *const AtomicU32);

        // CAS loop: standard lock-free stack push
        loop {
            let current_head = free_list.load(Ordering::Acquire);

            // Point current page at old head
            page_next_atomic.store(current_head, Ordering::Relaxed);

            // Try to make free list head point to current page
            if free_list
                .compare_exchange(
                    current_head,
                    page_offset,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                break;
            }
            // Retry on failure
        }
    }
}
