// Full-mesh RDMA demo — every node writes directly to every other node.
//
// Each node has a direct QP to every peer. No hub, no relay.
// Run one instance per node (on the same or different machines).
//
// Usage:
//   cargo run --example rdma_mesh_node -- <node_id> <total> <ip0> [ip1] ...
//
// Local test with 3 nodes (3 separate terminals):
//   cargo run --example rdma_mesh_node -- 0 3 127.0.0.1 127.0.0.1 127.0.0.1
//   cargo run --example rdma_mesh_node -- 1 3 127.0.0.1 127.0.0.1 127.0.0.1
//   cargo run --example rdma_mesh_node -- 2 3 127.0.0.1 127.0.0.1 127.0.0.1
//
// Real multi-machine (node 0 on .10, node 1 on .11, node 2 on .12):
//   # on .10:  cargo run --example rdma_mesh_node -- 0 3 192.168.1.10 192.168.1.11 192.168.1.12
//   # on .11:  cargo run --example rdma_mesh_node -- 1 3 192.168.1.10 192.168.1.11 192.168.1.12
//   # on .12:  cargo run --example rdma_mesh_node -- 2 3 192.168.1.10 192.168.1.11 192.168.1.12

use anyhow::Result;
use connect::MeshNode;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);

    let node_id: usize = args.next()
        .and_then(|s| s.parse().ok())
        .expect("usage: rdma_mesh_node <node_id> <total> <ip0> [ip1] ...");
    let total: usize = args.next()
        .and_then(|s| s.parse().ok())
        .expect("missing <total>");
    let ips: Vec<String> = args.collect();

    assert_eq!(ips.len(), total, "need exactly {} IP arguments", total);
    let ip_refs: Vec<&str> = ips.iter().map(|s| s.as_str()).collect();

    println!("=== RDMA Mesh Node {} / {} ===", node_id, total);

    // ── Phase 1: connect to all peers ────────────────────────────────────────

    let mut node = MeshNode::connect_all(node_id, total, &ip_refs)?;
    println!("[node {}] all {} peers connected", node_id, total - 1);

    // ── Phase 2: write state to every peer directly (no hub) ─────────────────

    let hostname = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "unknown\n".into());
    let hostname = hostname.trim();

    for peer in (0..total).filter(|&j| j != node_id) {
        let payload = format!("node={} host={} -> peer={}", node_id, hostname, peer);
        node.write_to(peer, payload.as_bytes())?;
    }

    // ── Phase 3: wait for all peers to finish writing ─────────────────────────

    node.wait_all_writes()?;

    // ── Phase 4: print what each peer wrote ───────────────────────────────────

    println!("\n=== Node {} received state from all peers ===", node_id);
    for peer in (0..total).filter(|&j| j != node_id) {
        println!("  slot[{}] = {:?}", peer, node.slot_str(peer));
    }

    Ok(())
}
