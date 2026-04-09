//! Integration test: spin up a mock SCX stats UNIX socket, verify the full
//! client -> parse -> cluster view -> advisor pipeline.

use scheduler::{ScxClusterView, ScxStatsClient};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::thread;

/// Sample SCX stats JSON resembling scx_rusty output (must be single-line for the line-based protocol).
const MOCK_STATS_JSON: &str = r#"{"at_us":1700000000000000,"cpu_busy":62.5,"load":245.8,"nr_migrations":312,"slice_us":20000,"time_used":0.032,"nodes":{"0":{"load":130.2,"imbal":7.3,"doms":{"0":{"load":65.1,"imbal":3.6}}},"1":{"load":115.6,"imbal":-7.3,"doms":{"0":{"load":57.8,"imbal":-3.6}}}}}"#;

/// Start a mock SCX stats server. Returns a join handle and signals readiness via the channel.
fn start_mock_server(socket_path: &str) -> (thread::JoinHandle<()>, mpsc::Receiver<()>) {
    let path = socket_path.to_string();
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path).expect("bind mock socket");
    let (tx, rx) = mpsc::channel();

    let handle = thread::spawn(move || {
        // Signal that the listener is ready.
        tx.send(()).unwrap();

        // Accept one connection, respond to one request, then exit.
        if let Ok((stream, _)) = listener.accept() {
            let write_stream = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();

            // Read the request line.
            if reader.read_line(&mut line).is_ok() {
                let response = format!(
                    r#"{{"errno":0,"args":{{"resp":{}}}}}"#,
                    MOCK_STATS_JSON
                );
                let mut writer = write_stream;
                let _ = writeln!(writer, "{}", response);
                let _ = writer.flush();
            }
        }
    });

    (handle, rx)
}

fn unique_socket(name: &str) -> String {
    format!(
        "/tmp/test_scx_{}_{}.sock",
        name,
        std::process::id()
    )
}

#[test]
fn test_client_fetches_from_mock_socket() {
    let socket_path = unique_socket("fetch");
    let (handle, ready) = start_mock_server(&socket_path);
    ready.recv().unwrap(); // Wait for server to be listening.

    let client = ScxStatsClient::new(Some(&socket_path));
    let snap = client.fetch().expect("should fetch stats from mock server");

    assert!((snap.cpu_busy - 62.5).abs() < 0.01);
    assert!((snap.load - 245.8).abs() < 0.01);
    assert_eq!(snap.nr_migrations, 312);
    assert_eq!(snap.slice_us, 20000);
    assert_eq!(snap.numa_nodes.len(), 2);
    assert!((snap.numa_nodes[&0].load - 130.2).abs() < 0.01);
    assert!((snap.numa_nodes[&1].imbal - (-7.3)).abs() < 0.01);

    handle.join().unwrap();
    let _ = std::fs::remove_file(&socket_path);
}

#[test]
fn test_client_returns_none_when_no_server() {
    let client = ScxStatsClient::new(Some("/tmp/nonexistent_scx_socket_12345.sock"));
    assert!(client.fetch().is_none());
}

#[test]
fn test_full_pipeline_mock_to_advisor() {
    let socket_path = unique_socket("pipeline");

    // Node 0: high load (from mock server).
    let (handle, ready) = start_mock_server(&socket_path);
    ready.recv().unwrap();

    let client = ScxStatsClient::new(Some(&socket_path));
    let snap_node0 = client.fetch().expect("fetch node 0 stats");
    handle.join().unwrap();
    let _ = std::fs::remove_file(&socket_path);

    // Node 1: manually create a lighter snapshot.
    let snap_node1 = scheduler::ScxNodeSnapshot {
        cpu_busy: 15.0,
        load: 50.0,
        nr_migrations: 20,
        slice_us: 20000,
        time_used: 0.01,
        numa_nodes: {
            let mut m = std::collections::BTreeMap::new();
            m.insert(0, scheduler::ScxNumaStats { load: 50.0, imbal: 0.5 });
            m
        },
    };

    // Build cluster view: node 0 is busy with high memory, node 1 is idle with low memory.
    let mut view = ScxClusterView::new();
    view.update(0, Some(snap_node0), 8_000_000_000, true, Some("job_42".into()), 1000);
    view.update(1, Some(snap_node1), 500_000_000, false, None, 1000);

    // Run advisor.
    let scores = scheduler::score_nodes(&view);
    assert_eq!(scores.len(), 2);

    // Node 1 should score better (lower) — less loaded.
    assert_eq!(scores[0].0, 1, "node 1 should be preferred (less loaded)");
    assert_eq!(scores[1].0, 0, "node 0 should be second (more loaded)");

    println!("=== Advisor Scores ===");
    for (id, score) in &scores {
        println!("  node {}: score={:.4}", id, score);
    }

    let best = scheduler::advisor::best_nodes(&view, 1);
    assert_eq!(best, vec![1]);
}
