// RDMA spoke demo — connects to a hub and writes this node's state.
//
// Each spoke connects to BASE_PORT + spoke_id on the hub.  The payload
// contains the spoke ID and hostname so the hub can tell them apart.
//
// Usage:
//   cargo run --example rdma_spoke -- <hub_ip> <spoke_id>
//
// Examples (run these after starting the hub):
//   cargo run --example rdma_spoke -- 127.0.0.1 0
//   cargo run --example rdma_spoke -- 127.0.0.1 1
//   cargo run --example rdma_spoke -- 192.168.1.10 2   # from a real remote node

use anyhow::Result;
use connect::RdmaRemote;

const BASE_PORT: u16   = 7480;
const SLOT_SIZE: usize = 256;

fn main() -> Result<()> {
    let hub_ip  = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1".to_owned());
    let spoke_id: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let port = BASE_PORT + spoke_id as u16;
    let hostname = hostname();

    println!("=== RDMA Spoke {} ===", spoke_id);
    println!("Connecting to {}:{}", hub_ip, port);

    let mut conn = RdmaRemote::connect(&hub_ip, port, SLOT_SIZE)?;

    let payload = format!("spoke={} host={} state=ready", spoke_id, hostname);
    println!("[spoke {}] writing: {:?}", spoke_id, payload);
    conn.write_state(payload.as_bytes())?;

    println!("[spoke {}] done", spoke_id);
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "unknown".into())
        .trim()
        .to_owned()
}
