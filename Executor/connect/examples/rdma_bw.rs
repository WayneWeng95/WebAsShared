// rdma_bw — multi-node one-directional RDMA *bandwidth* for the StateSync-remote
// micro-benchmark.  The companion to `rdma_latency` (which measures a ping-pong
// RTT/2): this one streams many pipelined one-sided RDMA WRITEs in a single
// direction and reports achieved GiB/s, so the headline bandwidth is not dragged
// down by a per-transfer host memcpy + TCP done-signal the way the latency
// ping-pong is.
//
// It is deliberately topology-agnostic — two roles compose into every pattern:
//
//   recv <port> [streams]   passive sink.  `streams=1` (default) serves one
//                           inbound stream; `streams=N` accepts N senders on the
//                           same port (the incast receiver).
//   send <ip...> [flags]    active source.  One outbound stream per IP, run
//                           concurrently with a barrier so the timed region
//                           overlaps; reports per-link and aggregate GiB/s.
//
// Patterns on the 4-node fabric (10.10.1.2/.1/.3/.4), per the hardware: each
// node has ONE 10 GbE experiment port (~1.13 GiB/s ceiling), so:
//
//   pair (1<->1)        — clean single-link bandwidth (≈ line rate):
//       node B:  rdma_bw recv 7900
//       node A:  rdma_bw send 10.10.1.1 --tag pair --csv bw.csv
//
//   fan-out (1->N)      — one sender's egress port shared across N peers
//                         (aggregate stays ≈ one port; each peer gets ~1/N):
//       nodes B,C,D: rdma_bw recv 7900
//       node A:      rdma_bw send 10.10.1.1 10.10.1.3 10.10.1.4 --tag fanout3 --csv bw.csv
//
//   incast (N->1)       — N senders into one receiver's ingress port
//                         (each sender reports ~1/N; sum ≈ one port):
//       node B:        rdma_bw recv 7900 3
//       nodes A,C,D:   rdma_bw send 10.10.1.1 --tag incast --csv bw.csv
//
//   disjoint pairs      — independent pairs on distinct nodes scale ≈ linearly
//   (the scaling story)   (run two `pair` benchmarks at once; sum the results):
//       node B: rdma_bw recv 7900     node D: rdma_bw recv 7900
//       node A: rdma_bw send 10.10.1.1   node C: rdma_bw send 10.10.1.4
//
// Build:  cargo build --release -p connect --example rdma_bw

use std::sync::{Arc, Barrier};
use std::time::Instant;

use anyhow::{bail, Result};
use connect::RdmaRemote;

// Size schedule — kept identical to rdma_latency / StateSync so figures align.
const SIZES: &[usize] = &[16 * 1024, 1024 * 1024, 16 * 1024 * 1024, 128 * 1024 * 1024];
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let role = args.first().map(String::as_str).unwrap_or("");
    let max_size = *SIZES.iter().max().unwrap();

    match role {
        "recv" => {
            let port: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(7900);
            let streams: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            run_recv(port, streams, max_size)?;
        }
        "send" => run_send(&args[1..], max_size)?,
        _ => bail!(
            "usage: rdma_bw <recv <port> [streams] | \
             send <ip...> [--port P] [--tag NAME] [--csv path] [--gib N]>"
        ),
    }
    Ok(())
}

// ── Receiver ────────────────────────────────────────────────────────────────────
/// Passive sink.  `streams == 1` serves a single inbound stream; `streams > 1`
/// accepts that many senders on one port (incast), each handled on its own
/// thread that simply blocks until its sender signals the stream is finished.
fn run_recv(port: u16, streams: usize, max_size: usize) -> Result<()> {
    if streams <= 1 {
        let mut r = RdmaRemote::serve(port, max_size)?;
        println!("[rdma_bw] recv ready on :{port} (1 stream) — waiting for sender");
        r.wait_peer_write()?; // blocks until the sender's signal_done()
        println!("[rdma_bw] recv done");
        return Ok(());
    }

    use std::net::TcpListener;
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    println!("[rdma_bw] recv listening on :{port} for {streams} streams (incast)");
    let mut handles = Vec::with_capacity(streams);
    for i in 0..streams {
        // Accept + RDMA setup happen sequentially on this thread; the per-stream
        // thread then just waits for that sender to finish.
        let mut r = RdmaRemote::serve_on(&listener, max_size)?;
        println!("[rdma_bw] stream {}/{} connected", i + 1, streams);
        handles.push(std::thread::spawn(move || -> Result<()> {
            r.wait_peer_write()
        }));
    }
    for h in handles {
        h.join().expect("recv stream thread panicked")?;
    }
    println!("[rdma_bw] recv done ({streams} streams)");
    Ok(())
}

// ── Sender ──────────────────────────────────────────────────────────────────────
struct SendCfg {
    ips:  Vec<String>,
    port: u16,
    tag:  String,
    csv:  Option<String>,
    gib:  f64, // data budget per size, per link (GiB)
}

fn run_send(args: &[String], max_size: usize) -> Result<()> {
    let cfg = parse_send(args)?;
    let n = cfg.ips.len();
    if n == 0 {
        bail!("send: need at least one peer IP");
    }
    println!(
        "[rdma_bw] send tag={} links={} -> {:?}  budget={:.1} GiB/size/link",
        cfg.tag, n, cfg.ips, cfg.gib
    );

    // Barrier aligns the timed region of every link per size, so concurrent
    // links genuinely contend (the point of fan-out).
    let barrier = Arc::new(Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for ip in cfg.ips.clone() {
        let barrier = Arc::clone(&barrier);
        let port = cfg.port;
        let gib = cfg.gib;
        handles.push(std::thread::spawn(move || -> Result<Vec<(usize, usize, f64)>> {
            let mut r = RdmaRemote::connect(&ip, port, max_size)?;
            let mut rows = Vec::with_capacity(SIZES.len());
            for &size in SIZES {
                let iters = plan_iters(size, gib);
                let warm = (iters / 10).clamp(2, 500);
                r.stream_write(size, warm)?; // warmup (untimed)
                barrier.wait(); // start the timed region together across links
                let t0 = Instant::now();
                r.stream_write(size, iters)?;
                rows.push((size, iters, t0.elapsed().as_secs_f64()));
            }
            r.signal_done()?;
            Ok(rows)
        }));
    }

    let mut per_link = Vec::with_capacity(n);
    for h in handles {
        per_link.push(h.join().expect("send link thread panicked")?);
    }

    // ── Aggregate + report ────────────────────────────────────────────────────
    println!(
        "{:>8}  {:>7}  {:>14}  {:>16}",
        "size", "links", "per-link GiB/s", "aggregate GiB/s"
    );
    let mut csv_rows = Vec::with_capacity(SIZES.len());
    for (si, &size) in SIZES.iter().enumerate() {
        let mut agg = 0.0;
        let mut iters = 0;
        for link in &per_link {
            let (sz, it, secs) = link[si];
            agg += (sz as f64 * it as f64) / secs / GIB;
            iters = it;
        }
        let per_link_avg = agg / n as f64;
        println!(
            "{:>8}  {:>7}  {:>14.3}  {:>16.3}",
            fmt_size(size), n, per_link_avg, agg
        );
        csv_rows.push(format!(
            "{},{},{},{},{:.4},{:.4}",
            cfg.tag, n, size, iters, per_link_avg, agg
        ));
    }

    if let Some(path) = cfg.csv.as_deref() {
        upsert_csv(path, &cfg.tag, &csv_rows)?;
        println!("[rdma_bw] upserted {} '{}' rows -> {}", csv_rows.len(), cfg.tag, path);
    }
    Ok(())
}

fn parse_send(args: &[String]) -> Result<SendCfg> {
    let mut ips = Vec::new();
    let mut port = 7900u16;
    let mut tag: Option<String> = None;
    let mut csv = None;
    let mut gib = 4.0f64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => { port = args.get(i + 1).and_then(|s| s.parse().ok())
                              .ok_or_else(|| anyhow::anyhow!("--port needs a value"))?; i += 2; }
            "--tag"  => { tag = args.get(i + 1).cloned(); i += 2; }
            "--csv"  => { csv = args.get(i + 1).cloned(); i += 2; }
            "--gib"  => { gib = args.get(i + 1).and_then(|s| s.parse().ok())
                              .ok_or_else(|| anyhow::anyhow!("--gib needs a value"))?; i += 2; }
            other if other.starts_with("--") => bail!("unknown flag {other}"),
            other => { ips.push(other.to_string()); i += 1; }
        }
    }
    let tag = tag.unwrap_or_else(|| if ips.len() == 1 { "link".into() }
                                    else { format!("fanout{}", ips.len()) });
    Ok(SendCfg { ips, port, tag, csv, gib })
}

/// Iterations to move ~`gib` GiB at `size` bytes each, clamped to a sane range.
fn plan_iters(size: usize, gib: f64) -> usize {
    let n = (gib * GIB / size as f64) as usize;
    n.clamp(5, 500_000)
}

/// Replace `tag`'s rows in the shared CSV (keeping all others), so every run /
/// topology accumulates into one file idempotently.
fn upsert_csv(path: &str, tag: &str, rows: &[String]) -> Result<()> {
    use std::io::Write;
    const HEADER: &str = "tag,n_links,size_bytes,iters,per_link_gibps,agg_gibps";
    let prefix = format!("{tag},");
    let mut kept: Vec<String> = Vec::new();
    if let Ok(existing) = std::fs::read_to_string(path) {
        for line in existing.lines().skip(1) {
            if !line.is_empty() && !line.starts_with(&prefix) {
                kept.push(line.to_string());
            }
        }
    }
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "{HEADER}")?;
    for line in &kept { writeln!(f, "{line}")?; }
    for row in rows { writeln!(f, "{row}")?; }
    Ok(())
}

fn fmt_size(n: usize) -> String {
    if n < 1024 { format!("{n}B") }
    else if n < 1024 * 1024 { format!("{}KiB", n / 1024) }
    else if n < 1024 * 1024 * 1024 { format!("{}MiB", n / (1024 * 1024)) }
    else { format!("{}GiB", n / (1024 * 1024 * 1024)) }
}
