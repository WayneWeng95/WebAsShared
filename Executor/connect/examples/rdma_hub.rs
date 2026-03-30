// RDMA hub demo — star topology, one hub accepting N spokes.
//
// The hub spawns one thread per spoke.  Each thread runs an independent
// RdmaRemote::serve() on its own port (BASE_PORT + spoke_id), so all N
// spokes can connect and write in parallel.  The hub prints every spoke's
// payload once all writes have landed.
//
// Usage:
//   cargo run --example rdma_hub -- <N>
//
// Example (3 spokes):
//   cargo run --example rdma_hub -- 3

use std::sync::mpsc;
use std::thread;

use anyhow::Result;
use connect::RdmaRemote;

const BASE_PORT: u16   = 7480;
const SLOT_SIZE: usize = 256;

fn main() -> Result<()> {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    println!("=== RDMA Hub: expecting {} spokes ===", n);
    println!("Ports {} .. {}", BASE_PORT, BASE_PORT + n as u16 - 1);

    let (tx, rx) = mpsc::channel::<(usize, String)>();

    // One thread per spoke — each listens on BASE_PORT + id.
    let handles: Vec<_> = (0..n).map(|id| {
        let tx = tx.clone();
        thread::spawn(move || -> Result<()> {
            let port = BASE_PORT + id as u16;
            let mut conn = RdmaRemote::serve(port, SLOT_SIZE)?;
            conn.wait_peer_write()?;
            let state = conn.local_state_str().to_string();
            tx.send((id, state)).ok();
            Ok(())
        })
    }).collect();

    drop(tx); // close our own sender so rx finishes when all threads are done

    // Collect results as they arrive (order depends on which spoke finishes first).
    let mut results = vec![String::new(); n];
    for (id, state) in rx {
        println!("[hub] spoke {} delivered: {:?}", id, state);
        results[id] = state;
    }

    for h in handles {
        h.join().ok();
    }

    println!("\n=== Hub summary ===");
    for (id, state) in results.iter().enumerate() {
        println!("  spoke {:2}: {}", id, state);
    }

    Ok(())
}
