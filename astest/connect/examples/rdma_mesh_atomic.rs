// Full-mesh RDMA atomic demo.
//
// All nodes simultaneously perform hardware-atomic Fetch-and-Add on a counter
// held in node 0's MR.  No locks, no CPU involvement on node 0 during the
// increments — the HCA handles atomicity end-to-end.
//
// Then every node does a Compare-and-Swap on a "ready flag" in node 0's slot
// to demonstrate CAS semantics.
//
// Layout in node 0's MR slot (byte offsets):
//   +0  : counter (u64) — every non-zero node FAA-increments this
//   +8  : flag    (u64) — every non-zero node CAS 0→node_id
//
// Usage (same as rdma_mesh_node):
//   cargo run --example rdma_mesh_atomic -- <node_id> <total> <ip0> [ip1] ...
//
// Local 3-node test:
//   T0: cargo run --example rdma_mesh_atomic -- 0 3 127.0.0.1 127.0.0.1 127.0.0.1
//   T1: cargo run --example rdma_mesh_atomic -- 1 3 127.0.0.1 127.0.0.1 127.0.0.1
//   T2: cargo run --example rdma_mesh_atomic -- 2 3 127.0.0.1 127.0.0.1 127.0.0.1

use anyhow::Result;
use connect::MeshNode;

const COUNTER_OFFSET: usize = 0;
const FLAG_OFFSET:    usize = 8;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let node_id: usize = args.next().and_then(|s| s.parse().ok())
        .expect("usage: rdma_mesh_atomic <node_id> <total> <ip0> ...");
    let total: usize = args.next().and_then(|s| s.parse().ok())
        .expect("missing <total>");
    let ips: Vec<String> = args.collect();
    assert_eq!(ips.len(), total);
    let ip_refs: Vec<&str> = ips.iter().map(|s| s.as_str()).collect();

    println!("=== RDMA Mesh Atomic Node {} / {} ===", node_id, total);

    let mut node = MeshNode::connect_all(node_id, total, &ip_refs)?;
    println!("[node {}] mesh ready", node_id);

    // ── Phase 1: all non-zero nodes atomically increment node 0's counter ─────

    if node_id != 0 {
        let old = node.fetch_and_add(0, COUNTER_OFFSET, 1)?;
        println!("[node {}] FAA counter on node 0: old={} new={}", node_id, old, old + 1);
    }

    // ── Phase 2: CAS — first writer sets flag; others see it was already set ──

    if node_id != 0 {
        let old = node.compare_and_swap(0, FLAG_OFFSET, 0, node_id as u64)?;
        if old == 0 {
            println!("[node {}] CAS flag on node 0: WON (flag was 0, set to {})",
                     node_id, node_id);
        } else {
            println!("[node {}] CAS flag on node 0: LOST (flag was already {})",
                     node_id, old);
        }
    }

    // ── Phase 3: node 0 waits briefly then reads its own memory ───────────────

    if node_id == 0 {
        // Wait for peers to signal that they are done (reuse the ctrl channel).
        node.wait_all_writes()?;

        let slot = node.slot(0);
        let counter = u64::from_ne_bytes(slot[COUNTER_OFFSET..COUNTER_OFFSET+8].try_into().unwrap());
        let flag    = u64::from_ne_bytes(slot[FLAG_OFFSET..FLAG_OFFSET+8].try_into().unwrap());

        println!("\n[node 0] final counter = {} (expected {})", counter, total - 1);
        println!("[node 0] final flag    = {} (first writer wins)", flag);
    } else {
        // Signal node 0 that this node's atomics are done.
        node.write_to(0, b"done")?;
    }

    Ok(())
}
