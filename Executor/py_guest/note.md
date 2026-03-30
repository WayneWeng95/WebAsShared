Due to the lack of support of the mmap in wasi, it need to use the poorly performed wasmtime I/O to load the memory region as a file into it. 

The performance hurts considerablily. If its possible to use just the RUST/C/C++ the memory operations is way better.


  There are 4 stacked overheads, each compounding on the next:

  1. Process spawn per node (biggest culprit)

  Every PyFunc node does std::process::Command::new("wasmtime").arg(python_wasm)...status(). With the word
  count demo that's 12 separate wasmtime run subprocess launches (distribute + 10 maps + reduce). 

  1. python.wasm JIT-compiled fresh every invocation

  wasmtime run compiles the python-3.12.0.wasm binary from scratch each time it starts. That's a ~50 MB WASM
  module being JIT'd 12 times per pipeline run. The Rust guest module is compiled once and reused.

  3. WASI file I/O instead of mmap

  shm.py uses seek + read for every SHM access because WASI has no mmap(2). Each _ru32() is two syscalls.
  Walking a page chain to read records does dozens of these. The Rust guest has SHM directly mapped into its
  address space — a pointer dereference, zero syscalls.

  4. Python interpreter warmup per invocation

  Each launch imports workloads, shm, struct, os, sys — all bytecode-compiled fresh. The actual word count
  logic is then also ~20-50x slower than native Rust for pure compute.

  ---
  One thing still tweakable (without changing the approach)

  If you haven't already: pre-AOT-compile python.wasm with wasmtime compile:

  wasmtime compile python-3.12.0.wasm -o python-3.12.0.cwasm

  Then reference the .cwasm in the DAG JSON. This eliminates the JIT cost on every invocation — it becomes a
  file load instead. It won't fix #1, #3, or #4, but the JIT overhead (#2) is a real chunk of each startup.

  Beyond that, you're right — the fundamental bottleneck is that the architecture spawns a fresh process for
  every node, which is the opposite of how the Rust WASM path works. That's the ceiling you've hit.