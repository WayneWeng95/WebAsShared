use crate::policy::{ConsumptionPolicy, ConsumptionResult, HostNode};
use crate::runtime::worker::WorkerState;
use std::sync::atomic::{AtomicU32, Ordering};
use wasmtime::{Memory, Store};

use common::{
    TARGET_OFFSET, REGISTRY_OFFSET, PAGE_SIZE, BUCKET_COUNT,
    Superblock, RegistryEntry, ChainNodeHeader,
};

pub struct BucketOrganizer<'a> {
    base_ptr: *mut u8,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> BucketOrganizer<'a> {
    fn superblock(&self) -> &Superblock {
        unsafe { &*(self.base_ptr as *const Superblock) }
    }

    /// Creates a new `BucketOrganizer` anchored at the shared memory base pointer.
    pub fn new(store: &mut Store<WorkerState>, memory: &Memory) -> Self {
        let base_ptr = unsafe { memory.data_ptr(store).add(TARGET_OFFSET) };
        Self {
            base_ptr,
            _marker: std::marker::PhantomData,
        }
    }

    /// Scans every hash bucket, atomically detaches its conflict list, applies `policy` to
    /// select a winner, commits the winner to the Registry, and recycles all losing pages.
    /// Must be called after all writers have finished to ensure the bucket lists are stable.
    pub unsafe fn consume_all_buckets<P: ConsumptionPolicy>(&self, policy: P) {
        let map_base_offset = self.superblock().shared_map_base.load(Ordering::Relaxed);
        if map_base_offset == 0 {
            return;
        }

        for i in 0..BUCKET_COUNT {
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

    /// Applies `policy` to the already-detached linked list rooted at `head_offset`.
    /// Deserializes each node's multi-page payload, invokes the policy, stores the winning
    /// node's offset and length into the Registry, and frees all losing nodes' page chains.
    unsafe fn process_detached_list<P: ConsumptionPolicy>(&self, head_offset: u32, bucket_idx: usize, policy: &P) {
        let mut nodes = Vec::new();
        let mut current_offset = head_offset;

        while current_offset != 0 {
            let node_ptr = self.base_ptr.add(current_offset as usize);
            
            let writer_id = *(node_ptr.add(4) as *const u32);
            let data_len = *(node_ptr.add(8) as *const u32);
            let registry_index = *(node_ptr.add(12) as *const u32);
            let next_offset = (*(node_ptr as *const AtomicU32)).load(Ordering::Relaxed);

            
            let mut payload = Vec::with_capacity(data_len as usize);
            let mut lob_offset = current_offset;
            let mut bytes_read = 0;
            let mut is_head = true;

            while bytes_read < data_len as usize {
                let page_ptr = self.base_ptr.add(lob_offset as usize);
                let (header_size, next_page) = if is_head {
                    let hdr = &*(page_ptr as *const ChainNodeHeader);
                    (std::mem::size_of::<ChainNodeHeader>(), hdr.next_payload_page)
                } else {
                    (std::mem::size_of::<u32>(), *(page_ptr as *const u32))
                };

                let read_len = std::cmp::min(data_len as usize - bytes_read, PAGE_SIZE as usize - header_size);
                let chunk = std::slice::from_raw_parts(page_ptr.add(header_size), read_len);
                payload.extend_from_slice(chunk);

                bytes_read += read_len;
                lob_offset = next_page;
                is_head = false;
            }

            nodes.push(HostNode { offset: current_offset, writer_id, data_len, registry_index, payload });
            current_offset = next_offset;
        }

        let result = policy.process(&nodes);

        
        let free_lob_chain = |start_offset: u32| {
            let mut free_offset = start_offset;
            let mut is_head = true;
            while free_offset != 0 {
                let page_ptr = self.base_ptr.add(free_offset as usize);
                let next_free = if is_head {
                    (*(page_ptr as *const ChainNodeHeader)).next_payload_page
                } else {
                    *(page_ptr as *const u32)
                };
                self.push_to_free_list(free_offset);
                free_offset = next_free;
                is_head = false;
            }
        };

        
        if let ConsumptionResult::Winner(winner_id, _content) = result {
            let winner_node = nodes.iter().find(|n| n.writer_id == winner_id).unwrap();

            let registry_base = self.base_ptr.add(common::REGISTRY_OFFSET as usize);
            let entry_ptr = registry_base.add(winner_node.registry_index as usize * std::mem::size_of::<RegistryEntry>()) as *const RegistryEntry;
            let entry = &*entry_ptr;
            
            entry.payload_offset.store(winner_node.offset, Ordering::Release);
            entry.payload_len.store(winner_node.data_len, Ordering::Release);

            for node in &nodes {
                if node.offset != winner_node.offset {
                    free_lob_chain(node.offset); 
                }
            }
        } else {
            for node in &nodes { 
                free_lob_chain(node.offset); 
            }
        }
    }

    /// Returns every node in the chain starting at `list_head` to the free list.
    /// Walks the top-level `next_node` links of the conflict list (not overflow payload pages).
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

    /// Pushes a single page back onto the sharded free list.
    /// Delegates to `reclaimer::free_page_chain` so the shard selection,
    /// spin_loop backoff, and ABA documentation are all in one place.
    unsafe fn push_to_free_list(&self, page_offset: u32) {
        let splice_addr = self.base_ptr as usize;
        crate::runtime::mem_operation::reclaimer::free_page_chain(splice_addr, page_offset);
    }
}
