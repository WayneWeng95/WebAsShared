# remote

Zero-copy SHM slot transfer between mesh nodes via RDMA.

The two public entry points (`execute_remote_send` / `execute_remote_recv`) dispatch
to one of two protocol implementations depending on the `RemoteProtocol` field in the
DAG configuration.  Both protocols transfer raw SHM page data over the RDMA data path
and use a dedicated TCP control stream per transfer direction for handshake messages.

---

## File structure

```
remote/
├── mod.rs    — Public API (execute_remote_send / execute_remote_recv) + compat shim
├── sender_initiated.rs   — Sender-initiated (SI) protocol: send_si / recv_si
├── receiver_initiated.rs — Receiver-initiated (RI) protocol: send_ri / recv_ri
├── shm.rs    — SHM slot helpers: collect SGEs, alloc/link page chain
├── rdma.rs   — RDMA write helpers: page-chain write, flat write
└── OVERVIEW.md — This file
```

---

## Protocols

### Sender-initiated (SI, default) — `si.rs`

```
Sender                              Receiver
──────                              ────────
TCP send_u32(total_bytes)  ──▶      alloc page chain, set metadata,
                                    link chain to slot (head/tail, Release)
                           ◀──      TCP send_u32(dest_off)
RDMA write into page chain ──▶      HCA writes chunks into page[i].data
TCP send_done              ──▶      worker can now read from slot
```

The receiver allocates and structures the page chain before any data arrives,
so the RDMA WRITE lands directly into the final layout.  Simple and efficient
for any unidirectional transfer.

### Receiver-initiated (RI) — `ri.rs`

```
Receiver                            Sender
────────                            ──────
TCP send(dest_off, avail_cap) ──▶   recv (dest_off, avail_cap)
                                    RDMA write to flat buffer at dest_off
                                    TCP send(total_bytes)         ──▶
                           ◀──      recv total_bytes
set page chain metadata in-place
link chain to slot
```

The receiver announces its current bump pointer and available capacity *first*,
before it knows the transfer size.  This eliminates the "who goes first" deadlock
in bidirectional (shuffle) DAGs where both sides would otherwise wait for the
other to announce before sending.

---

## File details

### mod.rs — Public API

| Symbol | Description |
|---|---|
| `execute_remote_send(splice_addr, slot, slot_kind, ch, protocol)` | Dispatch to `si::send_si` or `ri::send_ri` based on `protocol` |
| `execute_remote_recv(splice_addr, slot, slot_kind, ch, protocol)` | Dispatch to `si::recv_si` or `ri::recv_ri` based on `protocol` |
| `STAGE_BYTES_PER_PEER` | Compatibility constant (0 — no staging area needed) |
| `pre_alloc_staging(…)` | No-op kept for call-site compatibility |

### sender_initiated.rs — Sender-initiated protocol

| Function | Description |
|---|---|
| `send_si` | Collect source SGEs → announce `total_bytes` → receive `dest_off` → RDMA write page-chain → signal done |
| `recv_si` | Receive `total_bytes` → `alloc_and_link` → reply `dest_off` → wait for done signal |

### receiver_initiated.rs — Receiver-initiated protocol

| Function | Description |
|---|---|
| `send_ri` | Collect source SGEs → receive `dest_off` + `avail_cap` → RDMA flat write → send `total_bytes` as done |
| `recv_ri` | Announce `dest_off` + `avail_cap` → wait for `total_bytes` → reserve pages → structure page chain in-place → link to slot |

### shm.rs — SHM slot helpers

| Function | Description |
|---|---|
| `collect_src_sges(splice_addr, slot, slot_kind)` | Walk the slot's page chain and return `(Vec<(vaddr, len)>, total_bytes)` for use as RDMA source SGEs |
| `alloc_and_link(splice_addr, slot, slot_kind, total_bytes)` | Bump-allocate N pages, initialise each page's `cursor` and `next_offset`, link head/tail into slot atomics, return head offset |
| `link_to_slot(sb, slot, kind, head_off, tail_off)` | Store head/tail offsets into the appropriate `writer_heads`/`writer_tails` or `io_heads`/`io_tails` atomics with `Release` ordering |

### rdma.rs — RDMA write helpers

| Function | Description |
|---|---|
| `rdma_write_page_chain(ch, src_sges, remote_dest_off, total_bytes)` | SI path: split source SGEs across 4088-byte page boundaries, post one `ibv_sge` list per dest page, single `poll_one_blocking` at the end |
| `rdma_write_flat(ch, src_sges, remote_dest_off, total_bytes)` | RI path: chunk SGEs into `MAX_SEND_SGE`-sized batches, advance remote cursor after each batch, single `poll_one_blocking` at the end |

---

## Stream isolation

Each mesh peer pair maintains **two separate TCP control connections**:

- `ctrl_as_sender[peer]` — used exclusively by `RemoteSend` / `execute_remote_send`
- `ctrl_as_receiver[peer]` — used exclusively by `RemoteRecv` / `execute_remote_recv`

This means concurrent bidirectional transfers (e.g. two-node shuffle) never
interleave their TCP messages, and both SI and RI sends/recvs to the same peer
can run on different threads without locking beyond the per-stream `Mutex<TcpStream>`.
