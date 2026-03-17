// RDMA server demo.
//
// Registers a memory region, exchanges QP metadata with the client over TCP,
// transitions both QPs to RTS state, then waits for the client to RDMA WRITE
// a state update.  Prints the received bytes once the client signals done.
//
// Run BEFORE starting the client:
//   cargo run --example rdma_server

use anyhow::Result;
use connect::RdmaRemote;

const TCP_PORT:   u16   = 7474;
const STATE_SIZE: usize = 256;

fn main() -> Result<()> {
    println!("=== RDMA Server ===");
    println!("State buffer: {} bytes", STATE_SIZE);

    let mut remote = RdmaRemote::serve(TCP_PORT, STATE_SIZE)?;

    // Block until client sends its RDMA WRITE done signal.
    remote.wait_peer_write()?;

    let state = remote.local_state_str();
    println!("\n[server] received state ({} bytes):", state.len());
    println!("  \"{}\"", state);
    println!("\n[server] raw hex:");
    let raw = remote.local_state();
    let preview = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(32).min(32)];
    for (i, chunk) in preview.chunks(16).enumerate() {
        print!("  {:04x}: ", i * 16);
        for b in chunk { print!("{:02x} ", b); }
        println!();
    }

    Ok(())
}
