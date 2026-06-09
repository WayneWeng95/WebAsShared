# Intra-Wave Barrier Synchronization

Nodes in the same execution wave normally run in complete isolation — they
cannot observe each other's output until the wave completes.  The **barrier**
mechanism lifts this restriction: nodes that share a `barrier_group` can
synchronize mid-execution, allowing patterns like produce → exchange → consume
within a single wave.

## How it works

1. The host assigns each distinct `barrier_group` a sequential **barrier slot**
   (0, 1, ..., 63) in the Superblock and resets the counter to zero before the
   wave starts.

2. Guest workloads call `ShmApi::barrier_wait(barrier_id, party_count)`.
   Under the hood this is a Linux **futex** on the shared-memory counter:
   - Each caller atomically increments `barriers[barrier_id]`.
   - If it is not the last party, the process sleeps via `futex(FUTEX_WAIT)` —
     **zero CPU cost** while waiting.
   - The last party to arrive wakes all sleepers via `futex(FUTEX_WAKE)`.

3. After the barrier, all parties proceed and can safely read slots written by
   peers before the barrier.

Because the SHM file is `mmap`'d into every subprocess, the futex works
cross-process without any extra setup.

## DAG JSON

Add `"barrier_group"` to nodes that need to synchronize within a wave:

```json
{
  "shm_path": "/dev/shm/barrier_demo",
  "nodes": [
    { "id": "w0", "deps": [], "barrier_group": "sync1",
      "kind": { "WasmVoid": { "func": "collaborative_worker", "arg": 0 } } },
    { "id": "w1", "deps": [], "barrier_group": "sync1",
      "kind": { "WasmVoid": { "func": "collaborative_worker", "arg": 1 } } },
    { "id": "w2", "deps": [], "barrier_group": "sync1",
      "kind": { "WasmVoid": { "func": "collaborative_worker", "arg": 2 } } },
    { "id": "final", "deps": ["w0", "w1", "w2"],
      "kind": { "WasmVoid": { "func": "aggregate_results", "arg": 0 } } }
  ]
}
```

The host validates at load time that all nodes sharing a `barrier_group` land
in the same wave.  If dependency constraints push them into different waves, the
DAG is rejected with an error.

When the DAG starts, the host prints the barrier assignment:

```
[DAG] Barrier groups:
[DAG]   'sync1' → barrier_id=0, party_count=3
```

## Guest API

```rust
use api::ShmApi;

#[no_mangle]
pub extern "C" fn collaborative_worker(my_id: u32) {
    let slot = 100 + my_id as usize;
    let party_count = 3;

    // Phase 1: produce into my private slot.
    ShmApi::append_stream(slot, b"my partial result");

    // Synchronize — all 3 workers must reach here before any proceeds.
    ShmApi::barrier_wait(0, party_count);

    // Phase 2: safe to read peers' slots (100, 101, 102).
    for peer in 0..party_count {
        let peer_slot = 100 + peer as usize;
        // read peer_slot ...
    }
}
```

### Multiple barriers per function

A workload can call `barrier_wait` multiple times with different barrier IDs to
create multiple synchronization points:

```rust
ShmApi::barrier_wait(0, 3);  // barrier 0: after phase 1
// ... exchange data ...
ShmApi::barrier_wait(1, 3);  // barrier 1: after phase 2
// ... final aggregation ...
```

Each barrier ID must be unique within the DAG run.  Up to 64 barrier slots are
available (`BARRIER_COUNT`).

## Constraints

- **Same wave only**: nodes in a `barrier_group` must have compatible
  dependencies so the topological sort places them in the same wave.  If node B
  depends on node A, they will always be in different waves and cannot share a
  barrier group.

- **Subprocess nodes only**: barriers are meaningful for `WasmVoid`,
  `WasmU32`, `WasmFatPtr`, and `PyFunc` nodes, which run as parallel OS
  processes sharing the same SHM.  Host-side serial nodes (routing, I/O) run
  sequentially on the main thread and should not use barriers.

- **party_count must match**: every caller using the same `barrier_id` must
  pass the same `party_count`.  A mismatch will cause some processes to sleep
  forever (deadlock) or proceed too early.

- **Linux only**: the implementation relies on the `futex(2)` syscall.

## Why futex instead of spin-wait

Spin-waiting on a shared atomic burns CPU cycles on every core running a
waiting process, causing lock contention and reducing throughput.  The futex
approach puts waiters to sleep in the kernel scheduler — zero CPU cost while
waiting, immediate wakeup when the last party arrives.  This is the same
mechanism used by `pthread_barrier_wait` and Go's `sync.WaitGroup` on Linux.
