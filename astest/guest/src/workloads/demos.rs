use core::sync::atomic::Ordering;
use alloc::format;
use alloc::vec::Vec;
use crate::api::ShmApi;

static mut READ_BUFFER: Vec<u8> = Vec::new();

/// WASM export: writer workload for worker `id`.
/// Updates named atomics in the Registry, inserts a conflict node into the shared hash map,
/// and appends a JSON summary record to the worker's private stream.
#[no_mangle]
pub extern "C" fn writer(id: u32) {
    // [Plan A] 1. Distinct Global Counter for Requests (+1)
    let global_reqs = ShmApi::get_named_atomic("TotalRequests");
    let current_reqs = global_reqs.fetch_add(1, Ordering::SeqCst);

    // [Heap Logic] Simulate local computation
    let mut private_heap_data: Vec<u64> = Vec::new();
    for i in 1..=5 {
        private_heap_data.push((id as u64 * 1000) + i);
    }
    let local_sum: u64 = private_heap_data.iter().sum();

    // [Plan A] 2. Distinct Local Counter
    let local_name = format!("Worker_{}_Status", id);
    let local_counter = ShmApi::get_named_atomic(&local_name);
    let local_val = local_counter.fetch_add(1, Ordering::Relaxed);

    // [Plan A] 3. Distinct Global Batch Counter (+100)
    let global_batch = ShmApi::get_named_atomic("GlobalBatchCounter");
    let batch_val = global_batch.fetch_add(100, Ordering::SeqCst);

    // [Test] Collision on Key 888 (in Dynamic Page Pool Map)
    let shared_key = 888;
    let conflict_data = format!("Dynamic_Chain_Data_From_W{}", id);
    ShmApi::insert_shared_data(shared_key, id, conflict_data.as_bytes());

    let complex_data = format!(
        r#"{{"worker_id": {}, "local_status": {}, "global_reqs": {}, "global_batch": {}, "local_sum": {}}}"#,
        id, local_val, current_reqs, batch_val, local_sum
    );
    ShmApi::append_stream_data(id, complex_data.as_bytes());
}

/// WASM export: reads the latest record from writer `id`'s private stream.
/// Returns a packed `(ptr << 32 | len)` fat pointer for the host to read directly,
/// or `0` if no data is available yet.
#[no_mangle]
pub extern "C" fn reader(id: u32) -> u64 {
    if let Some((_origin, vec)) = ShmApi::read_latest_stream_data(id) {
        unsafe {
            READ_BUFFER = vec;
            let ptr = READ_BUFFER.as_ptr() as u64;
            let len = READ_BUFFER.len() as u64;
            (ptr << 32) | len
        }
    } else {
        0
    }
}

/// WASM export: returns the current value of registry atomic index 0 (`TotalRequests`).
#[no_mangle]
pub extern "C" fn read_live_global() -> u64 {
    ShmApi::get_atomic(0).load(Ordering::SeqCst)
}

/// WASM export: writes a finalized result string under the `"FuncA_Result"` task name.
#[no_mangle]
pub extern "C" fn func_a(id: u32) {
    let result = alloc::format!("This is the finalized data from Function A! (Winner ID: {})", id);
    ShmApi::write_shared_state("FuncA_Result", id, result.as_bytes());
    ShmApi::append_log(&alloc::format!("Func A (ID: {}) wrote output.\n", id));
}

/// WASM export: reads the Manager-committed result of `"FuncA_Result"` and logs it.
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

/// Node A: processes images and produces both stream output and shared-state stats.
#[no_mangle]
pub extern "C" fn process_image_node(id: u32) {
    let image_result = b"Binary_Image_Data...";
    ShmApi::append_stream_data(id, image_result);

    let progress_msg = format!("Worker {} finished batch.", id);
    ShmApi::write_shared_state("Global_Job_Status", id, progress_msg.as_bytes());
}

/// Node B: packs Node A's output.
#[no_mangle]
pub extern "C" fn zip_results_node(id: u32) {
    if let Some((_origin, _img_data)) = ShmApi::read_latest_stream_data(1) {
        // ... pack img_data ...
    }
    if let Some(_status) = ShmApi::read_shared_state("Global_Job_Status") {
        // ... check whether all tasks are done ...
    }
    let _ = id;
}

/// Fan-out demo: write the same record to three stream slots simultaneously.
#[no_mangle]
pub extern "C" fn produce_fan_out(id: u32) {
    let payload = alloc::format!("fan_out_src={},v={}", id, id * id);
    ShmApi::fan_out(&[id, id + 100, id + 200], payload.as_bytes());
}

/// Final output demo: compute a result and write it to the reserved output slot.
#[no_mangle]
pub extern "C" fn write_final_output(id: u32) {
    ShmApi::write_output_str(&alloc::format!(
        "worker={},result={},squared={}",
        id, id * 10, id * id
    ));
}
