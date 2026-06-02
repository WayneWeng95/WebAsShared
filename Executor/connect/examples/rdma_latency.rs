// rdma_latency — cross-node state-transfer latency for the StateSync-remote
// micro-benchmark (the `rdma-shm` approach).
//
// Ping-pong round-trip on the CLIENT, reported as one-way = RTT/2 (the
// ib_write_lat convention).  Each transfer is the system's real RemoteSend
// path: a memcpy into the registered MR + a one-sided RDMA WRITE into the
// peer's memory + the TCP done-signal the protocol uses to mark completion.
//
// Run the server on node B first, then the client on node A:
//   # node B (consumer, e.g. 10.10.1.1):
//   cargo run --release --example rdma_latency -- server 7900
//   # node A (producer, e.g. 10.10.1.2):
//   cargo run --release --example rdma_latency -- client 10.10.1.1 7900 --csv rdma_results.csv
//
// Loopback smoke test on one node (two terminals):
//   cargo run --release --example rdma_latency -- server 7900
//   cargo run --release --example rdma_latency -- client 127.0.0.1 7900
//
// Both roles MUST use the same size schedule (the consts below) so the
// ping-pong stays in lockstep.

use std::time::Instant;
use anyhow::{bail, Result};
use connect::RdmaRemote;

// Size schedule — must match ../StateSync-remote and StateSync-local.
const SIZES: &[usize] = &[16 * 1024, 1024 * 1024, 16 * 1024 * 1024, 128 * 1024 * 1024];
const ITERS: usize = 30;
const WARMUP: usize = 5;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let role = args.first().map(String::as_str).unwrap_or("");
    let max_size = *SIZES.iter().max().unwrap();

    match role {
        "server" => {
            let port: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(7900);
            let mut r = RdmaRemote::serve(port, max_size)?;
            run_server(&mut r)?;
            println!("[rdma_latency] server done");
        }
        "client" => {
            let host = args.get(1).map(String::as_str).unwrap_or("127.0.0.1");
            let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7900);
            let csv = flag(&args, "--csv");
            let mut r = RdmaRemote::connect(host, port, max_size)?;
            run_client(&mut r, csv.as_deref())?;
        }
        _ => bail!("usage: rdma_latency <server <port> | client <host> <port> [--csv path]>"),
    }
    Ok(())
}

/// Server (node B): for each size, echo every ping back to the client.
/// Untimed — its only job is to bounce the round-trip.
fn run_server(r: &mut RdmaRemote) -> Result<()> {
    let payload = vec![0xABu8; *SIZES.iter().max().unwrap()];
    for &size in SIZES {
        for _ in 0..(WARMUP + ITERS) {
            r.wait_peer_write()?;          // receive ping from client
            r.write_state(&payload[..size])?;  // echo back same size
        }
    }
    Ok(())
}

/// Client (node A): for each size, time `ITERS` ping-pong round-trips and
/// report one-way latency = RTT/2.
fn run_client(r: &mut RdmaRemote, csv: Option<&str>) -> Result<()> {
    let payload = vec![0xCDu8; *SIZES.iter().max().unwrap()];
    let mut rows: Vec<String> = Vec::new();

    println!("rdma-shm — one-way latency = RTT/2 (includes MR memcpy + RDMA WRITE + TCP done)");
    println!("{:>8}  {:>12}{:>12}{:>12}  {:>10}", "size", "mean us", "p50 us", "p99 us", "GiB/s");

    for &size in SIZES {
        // Warmup.
        for _ in 0..WARMUP {
            r.write_state(&payload[..size])?;
            r.wait_peer_write()?;
        }
        // Timed round-trips.
        let mut oneway_us: Vec<f64> = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            r.write_state(&payload[..size])?;   // ping  A -> B
            r.wait_peer_write()?;               // pong  B -> A
            let rtt = t0.elapsed().as_secs_f64() * 1e6;
            oneway_us.push(rtt / 2.0);
        }

        let mean = oneway_us.iter().sum::<f64>() / oneway_us.len() as f64;
        let p50 = pct(&oneway_us, 50.0);
        let p99 = pct(&oneway_us, 99.0);
        let gibps = size as f64 / (mean * 1e-6) / (1024.0 * 1024.0 * 1024.0);

        println!("{:>8}  {:>12.2}{:>12.2}{:>12.2}  {:>10.3}",
                 fmt_size(size), mean, p50, p99, gibps);
        // Same CSV schema as bench_remote.py so plot_remote.py reads both.
        rows.push(format!("rdma-shm,{},{},{:.4},{:.4},{:.4},{:.6}",
                          size, ITERS, mean, p50, p99, gibps));
    }

    if let Some(path) = csv {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        writeln!(f, "approach,size_bytes,iters,lat_mean_us,lat_p50_us,lat_p99_us,gibps")?;
        for row in &rows {
            writeln!(f, "{}", row)?;
        }
        println!("[rdma_latency] wrote {} rows -> {}", rows.len(), path);
    }
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────────────
fn pct(samples: &[f64], p: f64) -> f64 {
    if samples.is_empty() { return 0.0; }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let k = (((p / 100.0) * (s.len() as f64 - 1.0)).round() as usize).min(s.len() - 1);
    s[k]
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn fmt_size(n: usize) -> String {
    if n < 1024 { format!("{}B", n) }
    else if n < 1024 * 1024 { format!("{}KiB", n / 1024) }
    else if n < 1024 * 1024 * 1024 { format!("{}MiB", n / (1024 * 1024)) }
    else { format!("{}GiB", n / (1024 * 1024 * 1024)) }
}
