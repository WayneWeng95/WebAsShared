extern crate alloc;

mod shm_bridge;   // #[no_mangle] Rust fns called by shm_module.c
mod py_runner;    // MicroPython init + dispatch

// ─────────────────────────────────────────────────────────────────────────────
// C ABI exports — these are the DAG node entry points the host calls via
// Wasmtime, identical in signature to the Rust guest.
//
// Each function just initialises MicroPython (once) and dispatches to the
// corresponding Python function defined in python/workloads.py.
// ─────────────────────────────────────────────────────────────────────────────

// ── Word-count demo ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn wc_distribute(n_workers: u32) {
    py_runner::call_u32("wc_distribute", n_workers);
}

#[no_mangle]
pub extern "C" fn wc_map(slot: u32) {
    py_runner::call_u32("wc_map", slot);
}

#[no_mangle]
pub extern "C" fn wc_reduce(stream_slot: u32) {
    py_runner::call_u32("wc_reduce", stream_slot);
}

// ── Image pipeline demo ───────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn img_load_ppm(_: u32) {
    py_runner::call_u32("img_load_ppm", 0);
}

#[no_mangle]
pub extern "C" fn img_rotate(_: u32) {
    py_runner::call_u32("img_rotate", 0);
}

#[no_mangle]
pub extern "C" fn img_grayscale(_: u32) {
    py_runner::call_u32("img_grayscale", 0);
}

#[no_mangle]
pub extern "C" fn img_equalize(_: u32) {
    py_runner::call_u32("img_equalize", 0);
}

#[no_mangle]
pub extern "C" fn img_blur(_: u32) {
    py_runner::call_u32("img_blur", 0);
}

#[no_mangle]
pub extern "C" fn img_export_ppm(_: u32) {
    py_runner::call_u32("img_export_ppm", 0);
}

// ── Streaming pipeline demo ───────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn pipeline_source(out_slot: u32, round: u32) {
    py_runner::call_u32_u32("pipeline_source", out_slot, round);
}

#[no_mangle]
pub extern "C" fn pipeline_filter(in_slot: u32, out_slot: u32) {
    py_runner::call_u32_u32("pipeline_filter", in_slot, out_slot);
}

#[no_mangle]
pub extern "C" fn pipeline_transform(in_slot: u32, out_slot: u32) {
    py_runner::call_u32_u32("pipeline_transform", in_slot, out_slot);
}

#[no_mangle]
pub extern "C" fn pipeline_sink(in_slot: u32, summary_slot: u32) {
    py_runner::call_u32_u32("pipeline_sink", in_slot, summary_slot);
}
