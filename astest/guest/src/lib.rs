extern crate alloc;
mod api;

use core::sync::atomic::Ordering;
use api::ShmApi; 
use alloc::format;
use alloc::vec::Vec;

#[no_mangle]
pub extern "C" fn writer(id: u32) {
    ShmApi::append_log(&format!(">>> [INFO] Writer {} initialized.\n", id));


    let mut private_heap_data: Vec<u64> = Vec::new();
    
    for i in 1..=5 {
        private_heap_data.push((id as u64 * 1000) + i);
    }
    
    let local_sum: u64 = private_heap_data.iter().sum();

    let local_counter = ShmApi::get_atomic((id + 1) as usize);
    let global_counter = ShmApi::get_atomic(0);

    let local_val = local_counter.fetch_add(1, Ordering::Relaxed);
    let global_val = global_counter.fetch_add(100, Ordering::SeqCst);
    
    let complex_data = format!(
        r#"{{"worker_id": {}, "local_status": {}, "global_snapshot": {},"local_heap": {}}}"#, 
        id, local_val, global_val,local_sum
    );

    ShmApi::append_bytes(id, complex_data.as_bytes());      //Append the output into the memory region

    ShmApi::append_log(&format!("<<< [SUCCESS] Writer {} completed.\n", id));
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