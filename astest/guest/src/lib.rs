extern crate alloc;
mod api;

use core::sync::atomic::Ordering;
use api::ShmApi; 
use alloc::format;
use alloc::vec::Vec;

#[no_mangle]
pub extern "C" fn writer(id: u32) {
    // ShmApi::append_log(&format!(">>> [INFO] Writer {} initialized.\n", id));

    // [Plan A] 1. Distinct Global Counter for Requests (+1)
    // The Registry will assign a unique index for "TotalRequests" (likely Index 0)
    let global_reqs = ShmApi::get_named_atomic("TotalRequests");
    let current_reqs = global_reqs.fetch_add(1, Ordering::SeqCst);

    // [Heap Logic] Simulate local computation
    let mut private_heap_data: Vec<u64> = Vec::new();
    for i in 1..=5 {
        private_heap_data.push((id as u64 * 1000) + i);
    }
    let local_sum: u64 = private_heap_data.iter().sum();

    // [Plan A] 2. Distinct Local Counter
    // Instead of hardcoding (id + 1), use a dynamic name!
    // Example: "Worker_0_Status", "Worker_1_Status"...
    let local_name = format!("Worker_{}_Status", id);
    let local_counter = ShmApi::get_named_atomic(&local_name);
    let local_val = local_counter.fetch_add(1, Ordering::Relaxed);

    // [Plan A] 3. Distinct Global Batch Counter (+100)
    // This uses a DIFFERENT name, so Registry assigns a DIFFERENT index (likely Index 1 + 4 workers...)
    // No more aliasing with "TotalRequests"!
    let global_batch = ShmApi::get_named_atomic("GlobalBatchCounter");
    let batch_val = global_batch.fetch_add(100, Ordering::SeqCst);

    // 2. [Test] Collision on Key 888 (in Dynamic Page Pool Map)
    let shared_key = 888; 
    let conflict_data = format!("Dynamic_Chain_Data_From_W{}", id);
    ShmApi::insert_shared_data(shared_key, id, conflict_data.as_bytes());
    
    // Construct Payload with clearly separated metrics
    let complex_data = format!(
        r#"{{"worker_id": {}, "local_status": {}, "global_reqs": {}, "global_batch": {}, "local_sum": {}}}"#, 
        id, local_val, current_reqs, batch_val, local_sum
    );

    ShmApi::append_bytes(id, complex_data.as_bytes());

    // ShmApi::append_log(&format!("<<< [SUCCESS] Writer {} completed.\n", id));
}

// read buffer for OOM
static mut READ_BUFFER: Vec<u8> = Vec::new();

#[no_mangle]
pub extern "C" fn reader(id: u32) -> u64 {
    if let Some(vec) = ShmApi::read_latest_bytes(id) {
        unsafe {
            // take the read buffer
            READ_BUFFER = vec;
            // get the fat pointer
            let ptr = READ_BUFFER.as_ptr() as u64;
            let len = READ_BUFFER.len() as u64;
            (ptr << 32) | len
        }
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn read_live_global() -> u64 {
    ShmApi::get_atomic(0).load(Ordering::SeqCst)
}

// Func A (Writer)
#[no_mangle]
pub extern "C" fn func_a(id: u32) {
    let result = alloc::format!("This is the finalized data from Function A! (Winner ID: {})", id);
    
    ShmApi::write_shared_state("FuncA_Result", id, result.as_bytes());
    ShmApi::append_log(&alloc::format!("Func A (ID: {}) wrote output.\n", id));
}

// Func B (Reader)
#[no_mangle]
pub extern "C" fn func_b(_id: u32) {
    if let Some(input_data) = ShmApi::read_shared_state("FuncA_Result") {
        let text = alloc::string::String::from_utf8_lossy(&input_data);
        
       
        ShmApi::append_log(&alloc::format!("Func B received (len: {}): {}\n", input_data.len(), text));
        
        
        let mut hex_str = alloc::string::String::new();
        for &b in input_data.iter().take(16) {
            hex_str.push_str(&alloc::format!("{:02X} ", b));
        }
        ShmApi::append_log(&alloc::format!("Hex Dump (first 16 bytes): {}\n", hex_str));
        
    } else {
        ShmApi::append_log("Func B found no input!\n");
    }
}

/// Node A: processes images and produces both "processing result (Stream)" and "global state stats (Shared)"
#[no_mangle]
pub extern "C" fn process_image_node(id: u32) {
    // 1. Produce large business output (via Stream channel, no Manager involvement)
    let image_result = b"Binary_Image_Data...";
    ShmApi::append_stream_data(id, image_result);

    // 2. Report global task progress (via Shared channel, multi-Worker concurrent writes, Manager resolves LWW)
    let progress_msg = format!("Worker {} finished batch.", id);
    ShmApi::write_shared_state("Global_Job_Status", id, progress_msg.as_bytes());
}

/// Node B: packs Node A's output; it needs to read both kinds of data above
#[no_mangle]
pub extern "C" fn zip_results_node(id: u32) {
    // 1. Read Node A's (assume id 1) private output (directly from in-memory list, zero wait)
    if let Some(img_data) = ShmApi::read_stream_data(1) {
        // ... pack img_data ...
    }

    // 2. Read global task status (from Registry, Manager-confirmed data)
    if let Some(status) = ShmApi::read_shared_state("Global_Job_Status") {
        // ... check whether all tasks are done ...
    }
}