// RDMA client demo.
//
// Registers a memory region, connects to the server over TCP to exchange QP
// metadata, transitions both QPs to RTS state, then performs a one-sided
// RDMA WRITE to place a state payload directly into the server's memory —
// with no CPU involvement on the server during the transfer.
//
// Run AFTER starting the server:
//   cargo run --example rdma_client -- [server_ip]
// Defaults to localhost if no address is given.

use anyhow::Result;
use connect::RdmaRemote;

const TCP_PORT:   u16   = 7474;
const STATE_SIZE: usize = 256;

fn main() -> Result<()> {
    let server = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1".to_owned());

    println!("=== RDMA Client ===");
    println!("Connecting to {} : {}", server, TCP_PORT);

    let mut remote = RdmaRemote::connect(&server, TCP_PORT, STATE_SIZE)?;

    // Compose a state payload.
    let payload = b"hello from RDMA client — direct memory write, no server CPU";

    println!("\n[client] writing {} bytes via RDMA WRITE ...", payload.len());
    remote.write_state(payload)?;

    println!("[client] done — server memory has been updated without server CPU involvement");
    Ok(())
}
