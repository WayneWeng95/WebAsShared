# mesh

Full-mesh RDMA connections between N nodes.

Each ordered pair (i, j) shares one RC Queue Pair for RDMA WRITEs and two
separate TCP control streams — one per transfer direction — so concurrent
bidirectional transfers never interleave their messages.

---

## File structure

```
mesh/
├── mod.rs        — Types, constants, channel accessors, rand_psn
├── connect.rs    — Full-mesh connection setup (connect_all / connect_all_on_shm)
├── atomic.rs     — RDMA atomic ops (AtomicChannel + MeshNode SHM atomics)
├── data_path.rs  — Basic data-path: slot write/wait, u32 framing, slot reads
├── staging.rs    — Advanced write ops: scatter SGE, staging write/signal/wait, FAA-only
└── OVERVIEW.md   — This file
```

---

## mod.rs — Types and channel accessors

### Channel handle types

| Type | Description |
|---|---|
| `SendChannel` | Thread-safe handle for RDMA WRITEs to a peer. Holds `Arc<Mutex<TcpStream>>` (ctrl_as_sender) + `Arc<Mutex<QueuePair>>` + remote MR keys. Safe to move into `std::thread::spawn`. |
| `RecvChannel` | Thread-safe handle for receiving from a peer. Holds only `Arc<Mutex<TcpStream>>` (ctrl_as_receiver) — the receiver never posts RDMA WRs. |
| `AtomicChannel` | Thread-safe handle for one-sided RDMA atomics. Obtained via `MeshNode::atomic_channel`. |

### Internal types

| Type | Description |
|---|---|
| `PeerLink` | Per-peer state: QP, two TCP streams (sender / receiver), remote MR base, remote slot address, remote key. All fields `pub(in crate::mesh)` — visible only within the mesh module. |
| `MeshNode` | Full-mesh node holding `id`, `total`, and a `HashMap<usize, PeerLink>`. Private fields (`ctx`, `mr`, `peers`) are `pub(in crate::mesh)`. |

### Channel accessors

| Method | Description |
|---|---|
| `MeshNode::send_channel(peer_id)` | Clone Arc handles into a `SendChannel` for `peer_id`. Caller can move into a background thread while `MeshNode` stays on the main thread. |
| `MeshNode::recv_channel(peer_id)` | Clone the receiver TCP Arc into a `RecvChannel`. |
| `MeshNode::atomic_channel(peer_id)` | Clone QP and MR info into an `AtomicChannel`. |
| `MeshNode::mr_addr()` | Virtual base address of the local RDMA MR. |
| `MeshNode::mr_lkey()` | lkey of the local MR. |

---

## connect.rs — Full-mesh connection setup

### Port assignment

For each ordered pair (i, j) with i < j:

| Connection | Port formula | Role |
|---|---|---|
| conn-1 | `BASE_PORT + i*MAX_NODES + j` | QP metadata exchange; node i gets `ctrl_as_sender[j]`, node j gets `ctrl_as_receiver[i]` |
| conn-2 | `BASE_PORT2 + i*MAX_NODES + j` | Plain TCP ctrl only; node i gets `ctrl_as_receiver[j]`, node j gets `ctrl_as_sender[i]` |

### Public constructors

| Method | Description |
|---|---|
| `MeshNode::connect_all(node_id, total, ips)` | Allocate a small internal MR (`total * SLOT_SIZE` bytes) and establish connections with all peers. |
| `MeshNode::connect_all_on_shm(node_id, total, ips, shm_ptr, shm_len)` | Register an externally-owned SHM buffer as the RDMA MR. Used by the DAG runner so RDMA WRITEs land directly into the shared address space. |

---

## atomic.rs — RDMA atomic operations

### AtomicChannel methods

| Method | Description |
|---|---|
| `rdma_fetch_add(remote_off, result_off, add_val)` | RDMA FAA on `peer.shm[remote_off]`; result written to `self.shm[result_off]`. Returns old value. |
| `rdma_compare_swap(remote_off, result_off, compare, swap)` | RDMA CAS; swaps iff remote value equals `compare`. Returns old value. |

### MeshNode SHM atomic methods

Target any 8-byte-aligned offset in the full SHM MR (requires `connect_all_on_shm`).
Use `common::rdma_scratch_shm_offset(self.id, peer_id)` for a race-free result slot.

| Method | Description |
|---|---|
| `rdma_fetch_add_shm(peer_id, remote_off, result_off, add_val)` | FAA on any aligned offset in `peer_id`'s SHM. Takes `&self` — callable from shared references. |
| `rdma_compare_swap_shm(peer_id, remote_off, result_off, compare, swap)` | CAS on any aligned offset. Takes `&self`. |
| `fetch_and_add(peer_id, byte_offset, add_val)` | FAA within `peer_id`'s per-node MR slot (`byte_offset` relative to slot base). Takes `&mut self`. |
| `compare_and_swap(peer_id, byte_offset, compare, swap)` | CAS within `peer_id`'s per-node MR slot. Takes `&mut self`. |

---

## data_path.rs — Basic data-path

| Method | Description |
|---|---|
| `write_to(peer_id, data)` | Copy `data` into our MR slot, RDMA WRITE it into `peer_id`'s slot, then send a TCP done signal. |
| `broadcast(data)` | `write_to` every peer sequentially. |
| `wait_from(peer_id)` | Block until `peer_id` sends its done signal. |
| `wait_all_writes()` | Block until every peer has sent its done signal. |
| `slot(peer_id)` | Read-only view of the MR bytes written by `peer_id`. |
| `slot_str(peer_id)` | `slot(peer_id)` interpreted as a UTF-8 string (lossy, null-trimmed). |
| `signal_peer(peer_id)` | Send a one-byte TCP done signal without an RDMA WRITE. |
| `send_u32_to(peer_id, val)` | Write a little-endian `u32` to `peer_id` on the sender control stream. |
| `recv_u32_from(peer_id)` | Read a little-endian `u32` from `peer_id` on the receiver control stream. |

---

## staging.rs — Advanced write operations

| Method | Description |
|---|---|
| `rdma_write_sge(peer_id, remote_off, sge_pairs)` | Scatter-gather RDMA WRITE: post SGE-list batches of `MAX_SEND_SGE` entries, advancing the remote cursor after each batch. |
| `rdma_write_staging(peer_id, staging_offset, len)` | RDMA WRITE from a known SHM offset into the peer's matching offset. No TCP signal — use `rdma_signal_staging` afterward. |
| `rdma_signal_staging(peer_id, staging_offset)` | Increment `peer.shm[staging_offset]` via RDMA FAA (NIC-visible signal), then send a TCP done as a fallback. |
| `wait_staging(peer_id, counter_ptr)` | Spin-poll the atomic at `counter_ptr` for up to 100 ms; fall back to a TCP wait. |
| `rdma_faa_only(peer_id, remote_offset)` | FAA increment without any TCP signal — lower overhead for callers that poll the atomic directly. |
| `rdma_write_to_page_chain(peer_id, src_sges, remote_dest_off, total_bytes)` | Page-chain aware scatter write (legacy utility for callers that hold a `MeshNode` directly rather than a `SendChannel`). |
