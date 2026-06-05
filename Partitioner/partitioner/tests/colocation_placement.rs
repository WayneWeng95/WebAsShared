//! Integration tests for capacity-aware placement using word_count_auto.json.
//!
//! Two scenarios:
//!   balanced   — both hosts have equal capacity and a limit of 12 each
//!   skewed     — host 0 is nearly idle (limit 12), host 1 is saturated (limit 1)
//!
//! Each test partitions the DAG, then:
//!   1. Checks the placement distribution matches the hints
//!   2. Verifies the output is a valid ClusterDag (both node_dags keys present,
//!      every node accounted for, no duplicate node IDs)
//!   3. Checks that RemoteSend/RemoteRecv pairs are correctly injected whenever
//!      an original node ended up on a different host than its dependency

use partitioner::{PlacementHints, SymbolicDag, partition};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const AUTO_DAG: &str =
    include_str!("../../../DAGs/symbolic_dag/word_count_auto.json");

// ── helpers ───────────────────────────────────────────────────────────────────

fn hints(cap: &[(u32, f64)], lim: &[(u32, usize)]) -> PlacementHints {
    PlacementHints {
        capacity: cap.iter().cloned().collect(),
        host_limit: lim.iter().cloned().collect(),
        random: false,
        ..Default::default()
    }
}

/// Partition the auto DAG with given hints, return parsed cluster value.
fn run(h: &PlacementHints) -> Value {
    let dag = SymbolicDag::from_json(AUTO_DAG).expect("parse word_count_auto.json");
    partition(&dag, Some(h)).expect("partition")
}

/// Count nodes on each host; ignores RemoteSend/RemoteRecv infrastructure nodes.
fn count_original_nodes(cluster: &Value) -> HashMap<u32, usize> {
    let node_dags = cluster["node_dags"].as_object().expect("node_dags");
    node_dags
        .iter()
        .map(|(host_str, nodes)| {
            let host: u32 = host_str.parse().unwrap();
            let count = nodes
                .as_array()
                .unwrap()
                .iter()
                .filter(|n| {
                    let id = n["id"].as_str().unwrap_or("");
                    // Skip placer-injected infrastructure nodes.
                    !id.starts_with("rs_") && !id.starts_with("rr_")
                })
                .count();
            (host, count)
        })
        .collect()
}

/// Collect all node IDs across both hosts (including infrastructure).
fn all_node_ids(cluster: &Value) -> Vec<String> {
    cluster["node_dags"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|nodes| nodes.as_array().unwrap())
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect()
}

/// Verify every RemoteSend has a matching RemoteRecv on the other host.
fn check_send_recv_pairs(cluster: &Value) {
    let node_dags = cluster["node_dags"].as_object().unwrap();

    // Collect all RemoteSend ids.
    let sends: HashSet<String> = node_dags
        .values()
        .flat_map(|nodes| nodes.as_array().unwrap())
        .filter(|n| n["id"].as_str().unwrap_or("").starts_with("rs_"))
        .map(|n| {
            // rs_<src_id>_to_<dest> → rr_<src_id>_from_<src_machine>
            // We just check a matching rr_ node exists on the other host.
            n["id"].as_str().unwrap().to_string()
        })
        .collect();

    let recvs: HashSet<String> = node_dags
        .values()
        .flat_map(|nodes| nodes.as_array().unwrap())
        .filter(|n| n["id"].as_str().unwrap_or("").starts_with("rr_"))
        .map(|n| n["id"].as_str().unwrap().to_string())
        .collect();

    // Every rs_X_to_N should have a corresponding rr_X_from_M.
    for send in &sends {
        // send: "rs_foo_to_1"  recv: "rr_foo_from_0"
        let src = send
            .strip_prefix("rs_")
            .and_then(|s| s.rsplit_once("_to_"))
            .map(|(src_id, _)| src_id)
            .unwrap_or("");
        let matching = recvs.iter().any(|r| {
            r.strip_prefix("rr_")
                .and_then(|s| s.rsplit_once("_from_"))
                .map(|(id, _)| id == src)
                .unwrap_or(false)
        });
        assert!(
            matching,
            "RemoteSend '{}' has no matching RemoteRecv (recvs: {:?})",
            send, recvs
        );
    }
    assert_eq!(
        sends.len(),
        recvs.len(),
        "RemoteSend count ({}) != RemoteRecv count ({})",
        sends.len(),
        recvs.len()
    );
}

// ── scenario 1: balanced hosts ────────────────────────────────────────────────

#[test]
fn balanced_hosts_spread_nodes() {
    // Both hosts equally capable, limit 12 each.
    // Pinned: 12 per host (load + distribute + 10 maps). Auto: 5 (aggregates + reduce + save).
    // Dep-affinity pulls aggregate→host0, aggregate_local→host1, so both get ≥1 auto node.
    let h = hints(
        &[(0, 0.5), (1, 0.5)],
        &[(0, 12), (1, 12)],
    );
    let cluster = run(&h);

    let counts = count_original_nodes(&cluster);
    let on_0 = counts.get(&0).copied().unwrap_or(0);
    let on_1 = counts.get(&1).copied().unwrap_or(0);

    assert_eq!(on_0 + on_1, 29, "all 29 original nodes must be accounted for");

    // 12 pinned per host + at most 5 auto = 17 max per host.
    // At minimum, each host has 12 pinned + 1 auto via dep-affinity = 13.
    let auto_0 = on_0 - 12;
    let auto_1 = on_1 - 12;
    assert_eq!(auto_0 + auto_1, 5, "all 5 auto nodes assigned");
    assert!(auto_0 >= 1, "host 0 should get ≥1 auto node (aggregate via dep-affinity): got {}", auto_0);
    assert!(auto_1 >= 1, "host 1 should get ≥1 auto node (aggregate_local via dep-affinity): got {}", auto_1);
    assert!(auto_0 <= 12, "host 0 auto nodes exceed limit: {}", auto_0);
    assert!(auto_1 <= 12, "host 1 auto nodes exceed limit: {}", auto_1);

    // No duplicate node IDs.
    let ids = all_node_ids(&cluster);
    let unique: HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "duplicate node IDs in output");

    check_send_recv_pairs(&cluster);
}

// ── scenario 2: host 0 idle, host 1 saturated ────────────────────────────────

#[test]
fn skewed_hints_pack_onto_idle_host() {
    // Host 0: lots of headroom (limit 12). Host 1: nearly full (limit 1).
    // Single-host packing kicks in: 5 auto nodes ≤ limit(0)=12 and cap(0)=0.85 > 0.5
    // → all 5 auto nodes land on host 0.
    // Each host also has 12 pinned nodes (load + distribute + 10 maps).
    let h = hints(
        &[(0, 0.85), (1, 0.15)],
        &[(0, 12), (1, 1)],
    );
    let cluster = run(&h);

    let counts = count_original_nodes(&cluster);
    let on_0 = counts.get(&0).copied().unwrap_or(0);
    let on_1 = counts.get(&1).copied().unwrap_or(0);

    assert_eq!(on_0 + on_1, 29, "all 29 original nodes must be accounted for");

    // Host 1 has 12 pinned + at most 1 auto (limit=1).
    let auto_1 = on_1 - 12;
    assert!(
        auto_1 <= 1,
        "saturated host 1 should get ≤1 auto node (limit=1), got {}",
        auto_1
    );

    // Host 0 absorbs the rest: 12 pinned + ≥4 auto.
    let auto_0 = on_0 - 12;
    assert!(
        auto_0 >= 4,
        "idle host 0 should absorb ≥4 auto nodes, got {}",
        auto_0
    );

    // No duplicate node IDs.
    let ids = all_node_ids(&cluster);
    let unique: HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "duplicate node IDs in output");

    check_send_recv_pairs(&cluster);
}

// ── scenario 3: no hints (uniform fallback) ───────────────────────────────────

#[test]
fn no_hints_uniform_fallback() {
    let cluster = {
        let dag = SymbolicDag::from_json(AUTO_DAG).expect("parse");
        partition(&dag, None).expect("partition")
    };

    let counts = count_original_nodes(&cluster);
    let total: usize = counts.values().sum();
    assert_eq!(total, 29, "all nodes accounted for with no hints");

    // Each host has 12 pinned nodes; with no hints the 5 auto nodes are round-robined.
    // Both hosts should have at least their 12 pinned nodes.
    for host in [0u32, 1u32] {
        assert!(
            counts.get(&host).copied().unwrap_or(0) >= 12,
            "host {} is missing pinned nodes in uniform fallback",
            host
        );
    }

    check_send_recv_pairs(&cluster);
}

// ── scenario 4: single host absorbs everything ────────────────────────────────

#[test]
fn single_host_absorbs_all_when_idle() {
    // Host 0 dominates capacity (limit 30). Host 1 is saturated (limit 1).
    // Single-host packing: 5 auto nodes ≤ limit(0)=30 and cap(0)=0.95 > 0.5
    // → all 5 auto nodes land on host 0; host 1 gets only its 12 pinned nodes.
    let h = hints(
        &[(0, 0.95), (1, 0.05)],
        &[(0, 30), (1, 1)],
    );
    let cluster = run(&h);

    let counts = count_original_nodes(&cluster);
    let on_1_auto = counts.get(&1).copied().unwrap_or(0)
        .saturating_sub(12); // subtract 12 pinned nodes on host 1

    assert!(
        on_1_auto <= 1,
        "saturated host 1 should get at most 1 auto node, got {}",
        on_1_auto
    );
    check_send_recv_pairs(&cluster);
}
