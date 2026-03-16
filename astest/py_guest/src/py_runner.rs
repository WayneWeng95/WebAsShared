// MicroPython initialiser and function dispatcher.
//
// `init()` must be called once before any `call_*` variant.
// The interpreter is kept alive for the entire WASM module lifetime;
// Python globals (including workload function definitions) persist across calls.

use core::ffi::{c_char, c_int};

// ── MicroPython embed C API ───────────────────────────────────────────────────

extern "C" {
    /// Initialise the MicroPython interpreter with a caller-supplied heap.
    fn mp_embed_init(heap: *mut u8, heap_size: usize, stack_top: *mut u8);

    /// Execute a NUL-terminated string of Python source.
    /// Returns 0 on success, non-zero on exception.
    fn mp_embed_exec_str(src: *const c_char) -> c_int;
}

// ── Persistent heap for the MicroPython GC ───────────────────────────────────
// 256 KiB is comfortable for the demo workloads; increase if large dicts or
// string processing causes MicroPython to raise MemoryError.

const HEAP_SIZE: usize = 256 * 1024;
static mut MP_HEAP: [u8; HEAP_SIZE] = [0u8; HEAP_SIZE];

// Embed the Python workload source at compile time.
const WORKLOADS_PY: &str = include_str!("../python/workloads.py");

static mut INITIALIZED: bool = false;

// ── Init ──────────────────────────────────────────────────────────────────────

/// Initialise MicroPython and load the workload module.
/// No-op if already initialised.  Call before any `call_*` function.
pub fn init() {
    unsafe {
        if INITIALIZED { return; }

        // Use the address just past our stack frame as an approximation of
        // the C stack top (MicroPython uses this for stack-overflow detection).
        let stack_dummy: u8 = 0;
        mp_embed_init(
            MP_HEAP.as_mut_ptr(),
            HEAP_SIZE,
            &stack_dummy as *const u8 as *mut u8,
        );

        // Load the workload function definitions into the global namespace.
        let src = alloc::ffi::CString::new(WORKLOADS_PY)
            .expect("workloads.py must not contain interior NUL bytes");
        mp_embed_exec_str(src.as_ptr());

        INITIALIZED = true;
    }
}

// ── Dispatch helpers ──────────────────────────────────────────────────────────

/// Call a Python function that takes one u32 argument: `fn_name(arg)`.
pub fn call_u32(fn_name: &str, arg: u32) {
    init();
    let src = alloc::format!("{fn_name}({arg})\0");
    unsafe { mp_embed_exec_str(src.as_ptr() as *const c_char); }
}

/// Call a Python function that takes two u32 arguments: `fn_name(a, b)`.
pub fn call_u32_u32(fn_name: &str, a: u32, b: u32) {
    init();
    let src = alloc::format!("{fn_name}({a}, {b})\0");
    unsafe { mp_embed_exec_str(src.as_ptr() as *const c_char); }
}
