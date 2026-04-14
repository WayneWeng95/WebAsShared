# WASM64 (memory64) Support — Status and Notes

## Current Status: Infrastructure Complete, Runtime Blocked

The compile-time infrastructure for dual WASM32/WASM64 support is fully in place.
The WASM32 (default) path is unaffected and works identically to before.
The WASM64 path **compiles** (both host and guest) but **does not run** due to
wasmtime JIT compilation performance issues with 64-bit modules.

## What Was Done

### Compile-time feature flag (`wasm64`)

A `wasm64` Cargo feature propagates through all four crates:

```
common [features: wasm64]
   ├── guest  [features: wasm64 → common/wasm64]
   ├── host   [features: wasm64 → common/wasm64, connect/wasm64]
   └── connect [features: wasm64 → common/wasm64]
```

Under default features (no `wasm64`), all types resolve to their original `u32`/`AtomicU32`
values — zero behavioral change.

### Type aliases (common/src/lib.rs)

| Alias | wasm32 | wasm64 |
|-------|--------|--------|
| `ShmOffset` | `u32` | `u64` |
| `AtomicShmOffset` | `AtomicU32` | `AtomicU64` |
| `WasmPtr` | `u32` | `u64` |

### Struct changes

All SHM offset fields in `Superblock`, `Page`, `ChainNodeHeader`, and `RegistryEntry`
use `AtomicShmOffset`/`ShmOffset`. The `Page` struct has a compile-time assertion:

```rust
const _: () = assert!(core::mem::size_of::<Page>() == PAGE_SIZE as usize);
// wasm32: 4096 = 4 + 4 + 4088
// wasm64: 4096 = 8 + 8 + 4080
```

### Conditional constants

| Constant | wasm32 | wasm64 |
|----------|--------|--------|
| `TARGET_OFFSET` | 2 GiB (`0x8000_0000`) | 512 MiB (`0x2000_0000`) |
| `INITIAL_SHM_SIZE` | 36 MiB | 256 MiB |
| `REGISTRY_SIZE` | 1 MiB | 4 MiB |
| `ATOMIC_ARENA_SIZE` | 1 MiB | 8 MiB |
| `LOG_ARENA_SIZE` | 16 MiB | 64 MiB |
| `BUMP_SOFT_LIMIT` | `0x7FF0_0000` | `0xFFFF_FFF0_0000` |
| `CAPACITY_HARD_LIMIT` | 2 GiB | 256 TiB |
| `WASM_PATH` | `.../wasm32-.../guest.wasm` | `.../wasm64-.../guest.wasm` |

### Named constants replacing hardcoded values

| Old | New |
|-----|-----|
| `4088` | `PAGE_DATA_SIZE` |
| `PAGE_SIZE - 8` | `PAGE_DATA_SIZE` / `PAGE_HEADER_SIZE` |
| `0x7FF0_0000` | `BUMP_SOFT_LIMIT` |
| `0x8000_0000` (capacity guard) | `CAPACITY_HARD_LIMIT` |

### Host import ABI

| Import | wasm32 | wasm64 |
|--------|--------|--------|
| `host_remap(size)` | `u32` | `u64` (ShmOffset) |
| `host_resolve_atomic(ptr, len)` | `u32, u32` | `u64, u64` (WasmPtr) |
| `host_barrier_wait(id, count)` | `u32, u32` | `u32, u32` (unchanged) |

### Wasmtime engine config (wasm64)

```rust
config.wasm_memory64(true);
config.memory_reservation(16 * 1024 * 1024 * 1024); // 16 GiB virtual
```

### RDMA wire protocol

Added `send_shm_offset` / `recv_shm_offset` functions in `connect/src/rdma/exchange.rs`
that send 4 bytes (wasm32) or 8 bytes (wasm64) over TCP.

### Build system

```bash
# wasm32 (default, unchanged):
./build.sh

# wasm64:
WASM64=1 ./build.sh
```

`build.sh` generates the correct `guest/.cargo/config.toml` with the appropriate
target (`wasm32-unknown-unknown` or `wasm64-unknown-unknown`), `--max-memory`,
and `--initial-memory` linker flags.

### Wasmtime crate version

Upgraded from 16.0 to 29.0. Required for `wasm_memory64(true)` and 64-bit table
support in wasm64 modules.

## What Blocks Runtime Execution

### 1. JIT compilation is extremely slow for wasm64 modules

A single `wasm-call` subprocess with the wasm64 guest module takes **over 3 minutes**
just for JIT compilation before any user code executes. With 7 parallel workers
(as in the barrier test), this is impractical.

**Root cause:** wasmtime's Cranelift backend generates significantly more complex
code for 64-bit address spaces — every memory access needs wider bounds checks and
the instruction selection is more expensive.

**Potential fix:** AOT pre-compilation via `wasmtime compile guest.wasm -o guest.cwasm`.
The `.cwasm` file contains native machine code and loads in milliseconds. This was
not attempted.

### 2. Shared memory minimum size vs TARGET_OFFSET

For shared WASM memory (`MemoryType::shared`), wasmtime commits the `min` pages
upfront because shared memory cannot be relocated. If `TARGET_OFFSET` (where SHM
is mapped) exceeds the `min` allocation, the `MAP_FIXED` overlay lands outside
the WASM memory region and causes out-of-bounds traps.

The current compromise sets `TARGET_OFFSET` to 512 MiB and `min` to 1 GiB,
which leaves ~512 MiB for the guest heap. This is adequate but not the generous
"32 GiB for local use" originally envisioned.

**Constraint:** `wasm-ld` enforces a 16 GiB maximum for `--max-memory` on wasm64.
Combined with the physical commitment issue, the practical addressable SHM region
is ~15 GiB (16 GiB max minus guest heap).

### 3. Per-subprocess memory cost

Each `wasm-call` subprocess allocates its own WASM memory instance. With `min = 1 GiB`,
7 parallel workers commit ~7 GiB of physical pages (virtual on Linux with overcommit,
but still significant). The wasm32 path uses 4 GiB virtual per worker with only
~36 MiB physical.

## Files Modified

All changes are backward-compatible — under default features (`cargo build --release`),
every type, constant, and behavior is identical to the pre-wasm64 codebase.

| File | Changes |
|------|---------|
| `common/Cargo.toml` | `[features] wasm64 = []` |
| `common/src/lib.rs` | Type aliases, conditional constants, struct field types, `PAGE_DATA_SIZE`, compile-time assertions |
| `guest/Cargo.toml` | Feature forwarding |
| `guest/src/api/mod.rs` | `SHM_BASE`, `LOCAL_CAPACITY` use common constants |
| `guest/src/api/page_allocator.rs` | `ShmOffset` return type, named capacity guards |
| `guest/src/api/stream_area.rs` | `AtomicShmOffset` params, `ShmOffset` local vars |
| `guest/src/api/shared_area.rs` | `WasmPtr` params, `ShmOffset` offsets, `sizeof` for struct offsets |
| `guest/src/api/atomic_arena.rs` | `WasmPtr` params |
| `guest/src/api/log_arena.rs` | `ShmOffset` for log offset |
| `guest/src/workloads/demos.rs` | cfg-gated fatptr encoding |
| `guest/src/workloads/routing_tests.rs` | cfg-gated fatptr encoding |
| `host/Cargo.toml` | wasmtime 29.0, feature forwarding |
| `host/src/shm.rs` | `ShmOffset` field writes |
| `host/src/runtime/worker.rs` | Engine config, `MemoryType` builder, import ABI types |
| `host/src/routing/chain_splicer.rs` | `ShmOffset` params |
| `host/src/runtime/mem_operation/reclaimer.rs` | `ShmOffset` throughout |
| `host/src/runtime/mem_operation/organizer.rs` | `AtomicShmOffset`, `ShmOffset`, sizeof |
| `host/src/runtime/input_output/slot_loader.rs` | `ShmOffset` params/vars |
| `host/src/runtime/input_output/persistence.rs` | `ShmOffset` in PageReader |
| `host/src/runtime/input_output/logger.rs` | `ShmOffset` for log cursor |
| `host/src/runtime/remote/*.rs` | `ShmOffset` for offsets, wire protocol |
| `connect/Cargo.toml` | Feature forwarding |
| `connect/src/mesh/atomic.rs` | `ShmOffset` params |
| `connect/src/rdma/exchange.rs` | `send_shm_offset` / `recv_shm_offset` |
| `build.sh` | `WASM64=1` flag, config.toml generation |
| `scripts/rust.sh` | `wasm64-unknown-unknown` target |

## To Resume wasm64 Work

1. **AOT compilation**: Add `wasmtime compile` step to `build.sh` for wasm64 to
   eliminate JIT overhead. This is the most impactful fix.
2. **Memory strategy**: Investigate whether wasmtime's `memory_reservation` can
   be used with shared memory to reserve virtual space without committing physical
   pages, allowing a higher `TARGET_OFFSET`.
3. **Wasmtime upgrades**: Later wasmtime versions may improve wasm64 JIT performance
   and shared memory handling.
