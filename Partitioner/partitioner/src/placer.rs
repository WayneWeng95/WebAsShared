use std::collections::{HashMap, VecDeque};
use serde::{Deserialize, Serialize};
use rand::Rng;

use crate::symbolic_dag::{SymbolicNode, SplitHint};

/// Locality weight a producer's output edge exerts on its consumers in the
/// data-weighted dep-affinity (see `assign_nodes`).  Larger → the consumer is
/// pulled harder onto this node's host, so cross-machine cuts avoid this edge.
///
/// Resolution: the friendly `split` hint wins when present (`avoid` → a weight
/// that dominates all normal edges so the edge stays local; `prefer` → 0 so the
/// edge exerts no pull and becomes a cut point); otherwise the numeric
/// `out_weight`; otherwise `1.0` (the original count-based affinity).
///
/// `KEEP_WEIGHT` is a large *finite* sentinel: `avoid` is a strong preference,
/// not a hard guarantee, so it still yields to capacity/quota limits.
const KEEP_WEIGHT: f64 = 1e9;

pub(crate) fn edge_weight(node: &SymbolicNode) -> f64 {
    match node.split {
        Some(SplitHint::Avoid)  => KEEP_WEIGHT,
        Some(SplitHint::Prefer) => 0.0,
        None => node.out_weight.unwrap_or(1.0),
    }
}

/// Placement hints derived from live cluster state.
///
/// `capacity` holds per-host weights (normalized, sum = 1.0) that drive
/// proportional allocation.  `host_limit` holds the auto-derived maximum
/// number of sandboxes per host — computed as
/// `floor(cpu_cores × (1 − cpu_busy%)).max(1)` by the scheduler advisor.
///
/// Both maps may be empty (no sched_ext data available), in which case the
/// placer falls back to uniform round-robin with a limit of 1 per host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlacementHints {
    #[serde(default)]
    pub capacity: HashMap<u32, f64>,
    /// Per-host max sandboxes. Absent host → 1.
    #[serde(default)]
    pub host_limit: HashMap<u32, usize>,
    /// Per-host physical core count (from each node's reported `cpu_cores`).
    /// Used to cap a requested `fanout` against the cluster core budget
    /// (see `cap_fanout`).  Empty → no core data → fanout cap disabled.
    #[serde(default)]
    pub cores: HashMap<u32, usize>,
    /// When true, each auto-node is assigned to a uniformly random host.
    /// Dep-affinity and quota tracking are both skipped.
    #[serde(default)]
    pub random: bool,
}

/// Assign `node_id` for every node whose `node_id` is currently `None`.
///
/// Nodes with an explicit `node_id` are left untouched (pinned by the user).
///
/// Per-host limits come from `hints.host_limit` (auto-derived from CPU cores
/// and busy %).  `global_cap` is an optional hard upper bound applied on top
/// (from `SymbolicDag::max_colocation` when the user wants to constrain further).
///
/// Placement strategy:
/// 1. If all auto-nodes fit on the single best host within its limit, pack
///    them there to minimise cross-machine edges.
/// 2. Otherwise distribute proportionally to capacity weights (largest-
///    remainder method), then assign greedily in topological order with
///    dep-affinity tie-breaking.
/// 3. Falls back to round-robin with limit=1 when hints are empty.
pub fn assign_nodes(
    nodes: &mut [SymbolicNode],
    total_nodes: usize,
    hints: &PlacementHints,
    global_cap: Option<usize>,
) {
    if total_nodes == 0 {
        return;
    }

    let auto_count = nodes.iter().filter(|n| n.node_id.is_none()).count();
    if auto_count == 0 {
        return;
    }

    if hints.random {
        let mut rng = rand::thread_rng();
        for node in nodes.iter_mut().filter(|n| n.node_id.is_none()) {
            node.node_id = Some(rng.gen_range(0..total_nodes as u32));
        }
        return;
    }

    // Pre-seed the host map with already-pinned nodes so dep-affinity considers them.
    let mut id_to_host: HashMap<String, u32> = nodes
        .iter()
        .filter_map(|n| n.node_id.map(|id| (n.id.clone(), id)))
        .collect();

    // Locality hint: the breakability weight each node's output edge(s) exert.
    // Used to weight dep-affinity so a consumer is pulled toward the host of its
    // strongest-`avoid` / heaviest producer — cross-machine cuts then fall on
    // `prefer` / lightest edges.  Neutral nodes resolve to 1.0, reproducing the
    // original count-based affinity exactly (see `edge_weight`).
    let id_to_weight: HashMap<String, f64> = nodes
        .iter()
        .map(|n| (n.id.clone(), edge_weight(n)))
        .collect();

    let mut quota = compute_quotas(total_nodes, auto_count, hints, global_cap);

    let order = topo_order(nodes);

    for idx in order {
        if nodes[idx].node_id.is_some() {
            continue;
        }

        // Sum the data-weight of direct deps that landed on each host
        // (data-weighted dep-affinity). With default weights (1.0) this is the
        // original per-dep count.
        let mut affinity: HashMap<u32, f64> = HashMap::new();
        for dep_id in &nodes[idx].deps {
            if let Some(&host) = id_to_host.get(dep_id.as_str()) {
                let w = id_to_weight.get(dep_id.as_str()).copied().unwrap_or(1.0);
                *affinity.entry(host).or_insert(0.0) += w;
            }
        }

        // Pick the host with the most dep-affinity (heaviest local data) that
        // still has quota; break ties by remaining quota (packs onto fewer
        // hosts).  Ties return the last maximal host, matching the prior
        // `max_by_key` behaviour.
        let best = (0..total_nodes as u32)
            .filter(|h| quota.get(h).copied().unwrap_or(0) > 0)
            .max_by(|a, b| {
                let key_a = (affinity.get(a).copied().unwrap_or(0.0),
                             quota.get(a).copied().unwrap_or(0) as f64);
                let key_b = (affinity.get(b).copied().unwrap_or(0.0),
                             quota.get(b).copied().unwrap_or(0) as f64);
                key_a.partial_cmp(&key_b).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(0);

        nodes[idx].node_id = Some(best);
        id_to_host.insert(nodes[idx].id.clone(), best);
        // Decrement quota; saturating so that post-quota overflow-fallback
        // assignments (when all hosts hit 0) don't panic.
        quota.entry(best).and_modify(|q| *q = q.saturating_sub(1));
    }
}

// ── Quota computation ─────────────────────────────────────────────────────────

fn compute_quotas(
    total_nodes: usize,
    auto_count: usize,
    hints: &PlacementHints,
    global_cap: Option<usize>,
) -> HashMap<u32, usize> {
    // Per-host limit: from hints.host_limit, capped by global_cap if set.
    // When host_limit is entirely absent the intent is "no explicit limit" —
    // any host may receive all auto-placed nodes.  The fallback-to-1 only
    // applies when *some* limits are set but a particular host is missing
    // (treat it as the most conservative assumption).
    let limit = |host: u32| -> usize {
        let per_host = if hints.host_limit.is_empty() {
            auto_count
        } else {
            hints.host_limit.get(&host).copied().unwrap_or(1).max(1)
        };
        global_cap.map(|c| per_host.min(c)).unwrap_or(per_host)
    };

    if hints.capacity.is_empty() {
        // No capacity data: distribute uniformly.
        // If host_limit is also empty (no core data reported at all) give each
        // host the full auto_count so nothing is artificially capped.
        let base = if hints.host_limit.is_empty() {
            auto_count
        } else {
            (auto_count + total_nodes - 1) / total_nodes // ceiling
        };
        return (0..total_nodes as u32)
            .map(|i| (i, base.min(limit(i))))
            .collect();
    }

    // Single-host packing: put everything on the best host when it fits.
    let uniform_share = 1.0 / total_nodes as f64;
    if let Some((&best_id, &best_cap)) = hints
        .capacity
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
    {
        if auto_count <= limit(best_id) && best_cap > uniform_share {
            let mut quota = HashMap::new();
            quota.insert(best_id, auto_count);
            return quota;
        }
    }

    // Proportional allocation with largest-remainder rounding.
    let floor = uniform_share * 0.5;
    let mut raw: Vec<(u32, f64)> = (0..total_nodes as u32)
        .map(|i| (i, hints.capacity.get(&i).copied().unwrap_or(floor)))
        .collect();
    let total_cap: f64 = raw.iter().map(|(_, c)| c).sum();
    for (_, c) in &mut raw {
        *c /= total_cap;
    }

    let mut allocs: Vec<(u32, f64, usize)> = raw
        .iter()
        .map(|&(id, cap)| {
            let exact = cap * auto_count as f64;
            (id, exact, exact as usize)
        })
        .collect();

    // Largest-remainder: distribute residual slots to highest fractional parts.
    let floored: usize = allocs.iter().map(|(_, _, a)| a).sum();
    let leftover = auto_count.saturating_sub(floored);
    allocs.sort_by(|a, b| {
        let ra = a.1 - a.2 as f64;
        let rb = b.1 - b.2 as f64;
        rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
    });
    let len = allocs.len();
    for i in 0..leftover {
        allocs[i % len].2 += 1;
    }

    // Apply per-host limits.
    let mut quota: HashMap<u32, usize> =
        allocs.into_iter().map(|(id, _, a)| (id, a.min(limit(id)))).collect();

    // If limits created a deficit, top up the highest-capacity hosts.
    let capped_total: usize = quota.values().sum();
    if capped_total < auto_count {
        let mut deficit = auto_count - capped_total;
        let mut by_cap: Vec<u32> = (0..total_nodes as u32).collect();
        by_cap.sort_by(|a, b| {
            hints
                .capacity
                .get(b)
                .partial_cmp(&hints.capacity.get(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for host in by_cap {
            if deficit == 0 {
                break;
            }
            let current = quota.get(&host).copied().unwrap_or(0);
            let headroom = limit(host).saturating_sub(current);
            let add = headroom.min(deficit);
            *quota.entry(host).or_insert(0) += add;
            deficit -= add;
        }
    }

    quota
}

// ── Topological sort (Kahn's algorithm) ──────────────────────────────────────

pub(crate) fn topo_order(nodes: &[SymbolicNode]) -> Vec<usize> {
    let id_to_idx: HashMap<&str, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();

    let n = nodes.len();
    let mut in_degree = vec![0usize; n];
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, node) in nodes.iter().enumerate() {
        for dep_id in &node.deps {
            if let Some(&dep_idx) = id_to_idx.get(dep_id.as_str()) {
                in_degree[i] += 1;
                successors[dep_idx].push(i);
            }
        }
    }

    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, &d)| d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut order = Vec::with_capacity(n);
    while let Some(idx) = queue.pop_front() {
        order.push(idx);
        for &succ in &successors[idx] {
            in_degree[succ] -= 1;
            if in_degree[succ] == 0 {
                queue.push_back(succ);
            }
        }
    }

    // Append any remaining nodes (guards against cycles in malformed input).
    if order.len() < n {
        let in_order: std::collections::HashSet<usize> = order.iter().copied().collect();
        for i in 0..n {
            if !in_order.contains(&i) {
                order.push(i);
            }
        }
    }

    order
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbolic_dag::{SymbolicNode, SplitHint};
    use serde_json::json;

    fn make_node(id: &str, deps: Vec<&str>, node_id: Option<u32>) -> SymbolicNode {
        SymbolicNode {
            id: id.to_string(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            node_id,
            output_slot: None,
            fanout: None,
            placement: None,
            barrier_group: None,
            split: None,
            out_weight: None,
            kind: json!({"Func": {"slot": 0}}),
        }
    }

    fn hints_with_limits(cap: &[(u32, f64)], lim: &[(u32, usize)]) -> PlacementHints {
        PlacementHints {
            capacity: cap.iter().cloned().collect(),
            host_limit: lim.iter().cloned().collect(),
            random: false,
            ..Default::default()
        }
    }

    #[test]
    fn single_host_packing_when_idle() {
        // Host 1 has 70% capacity share and a limit of 4 — all 4 nodes should pack there.
        let mut nodes = vec![
            make_node("a", vec![], None),
            make_node("b", vec!["a"], None),
            make_node("c", vec!["a"], None),
            make_node("d", vec!["b", "c"], None),
        ];
        let hints = hints_with_limits(
            &[(0, 0.05), (1, 0.70), (2, 0.15), (3, 0.10)],
            &[(0, 1), (1, 4), (2, 2), (3, 2)],
        );

        assign_nodes(&mut nodes, 4, &hints, None);

        for n in &nodes {
            assert_eq!(n.node_id, Some(1), "expected all on host 1, got {:?}", n.node_id);
        }
    }

    #[test]
    fn proportional_spread_when_limited() {
        // 4 auto-nodes, 2 hosts, each limited to 2 → 2 per host.
        let mut nodes = vec![
            make_node("a", vec![], None),
            make_node("b", vec![], None),
            make_node("c", vec![], None),
            make_node("d", vec![], None),
        ];
        let hints = hints_with_limits(
            &[(0, 0.5), (1, 0.5)],
            &[(0, 2), (1, 2)],
        );

        assign_nodes(&mut nodes, 2, &hints, None);

        let on_0 = nodes.iter().filter(|n| n.node_id == Some(0)).count();
        let on_1 = nodes.iter().filter(|n| n.node_id == Some(1)).count();
        assert_eq!(on_0 + on_1, 4);
        assert!(on_0 <= 2 && on_1 <= 2, "host_limit violated: {}/{}", on_0, on_1);
    }

    #[test]
    fn global_cap_overrides_host_limit() {
        // Host 0 would allow 8 (from host_limit), but global_cap=2 should constrain it.
        let mut nodes = vec![
            make_node("a", vec![], None),
            make_node("b", vec![], None),
            make_node("c", vec![], None),
            make_node("d", vec![], None),
        ];
        let hints = hints_with_limits(
            &[(0, 0.9), (1, 0.1)],
            &[(0, 8), (1, 8)],
        );

        assign_nodes(&mut nodes, 2, &hints, Some(2));

        for host in [0u32, 1u32] {
            let count = nodes.iter().filter(|n| n.node_id == Some(host)).count();
            assert!(count <= 2, "global_cap=2 violated on host {}: {}", host, count);
        }
    }

    #[test]
    fn pinned_nodes_untouched() {
        let mut nodes = vec![
            make_node("a", vec![], Some(0)), // pinned
            make_node("b", vec!["a"], None),
        ];
        let hints = PlacementHints::default();
        assign_nodes(&mut nodes, 2, &hints, None);

        assert_eq!(nodes[0].node_id, Some(0), "pinned node must not change");
        assert!(nodes[1].node_id.is_some(), "auto node must be assigned");
    }

    #[test]
    fn split_avoid_keeps_consumer_local() {
        // `split: "avoid"` on a producer = don't break its output edge. The
        // consumer should colocate with it even though its other dep is on a
        // different host, so the cut lands on the OTHER (neutral) edge.
        let mut keep = make_node("keep", vec![], Some(0));
        keep.split = Some(SplitHint::Avoid);
        let other = make_node("other", vec![], Some(1)); // neutral
        let sink = make_node("sink", vec!["keep", "other"], None);
        let mut nodes = vec![keep, other, sink];

        let hints = PlacementHints::default(); // every host has full quota
        assign_nodes(&mut nodes, 2, &hints, None);

        assert_eq!(
            nodes[2].node_id, Some(0),
            "sink must stay with the `avoid` producer (host 0), got {:?}",
            nodes[2].node_id
        );
    }

    #[test]
    fn split_prefer_becomes_the_cut_point() {
        // `split: "prefer"` on a producer = a safe place to break. The consumer
        // is NOT pulled toward it, so it follows its neutral dep on the other
        // host and the `prefer` edge is the one that gets cut.
        let mut cheap = make_node("cheap", vec![], Some(0));
        cheap.split = Some(SplitHint::Prefer);
        let other = make_node("other", vec![], Some(1)); // neutral
        let sink = make_node("sink", vec!["cheap", "other"], None);
        let mut nodes = vec![cheap, other, sink];

        let hints = PlacementHints::default();
        assign_nodes(&mut nodes, 2, &hints, None);

        assert_eq!(
            nodes[2].node_id, Some(1),
            "sink should follow its neutral dep (host 1), cutting the `prefer` edge, got {:?}",
            nodes[2].node_id
        );
    }

    #[test]
    fn out_weight_pulls_consumer_to_heaviest_producer() {
        // Two pinned producers on different hosts feed one auto consumer.
        // The heavy edge should stay local: the sink lands on the heavy
        // producer's host, so the cross-machine cut falls on the LIGHT edge.
        let mut heavy = make_node("heavy", vec![], Some(0));
        heavy.out_weight = Some(5.0);
        let mut light = make_node("light", vec![], Some(1));
        light.out_weight = Some(1.0);
        let sink = make_node("sink", vec!["heavy", "light"], None);
        let mut nodes = vec![heavy, light, sink];

        // Empty hints → every host has full quota, so data-weighted dep-affinity
        // alone decides placement.
        let hints = PlacementHints::default();
        assign_nodes(&mut nodes, 2, &hints, None);

        assert_eq!(
            nodes[2].node_id, Some(0),
            "sink should colocate with the heavy producer (host 0), got {:?}",
            nodes[2].node_id
        );
    }

    #[test]
    fn equal_weights_reproduce_count_affinity() {
        // Sanity / backward-compat: with default (absent) weights the tie-break
        // is unchanged — equal affinity resolves to the last maximal host, exactly
        // as the original count-based `max_by_key` did.
        let p0 = make_node("p0", vec![], Some(0));
        let p1 = make_node("p1", vec![], Some(1));
        let sink = make_node("sink", vec!["p0", "p1"], None);
        let mut nodes = vec![p0, p1, sink];

        let hints = PlacementHints::default();
        assign_nodes(&mut nodes, 2, &hints, None);

        assert_eq!(
            nodes[2].node_id, Some(1),
            "equal-weight tie should resolve to the last host (unchanged behaviour)"
        );
    }

    #[test]
    fn dep_affinity_colocates_chain() {
        // a→b→c, host 0 is idle with limit 3 — all should colocate.
        let mut nodes = vec![
            make_node("a", vec![], None),
            make_node("b", vec!["a"], None),
            make_node("c", vec!["b"], None),
        ];
        let hints = hints_with_limits(
            &[(0, 0.80), (1, 0.20)],
            &[(0, 3), (1, 3)],
        );

        assign_nodes(&mut nodes, 2, &hints, None);

        let assigned: Vec<Option<u32>> = nodes.iter().map(|n| n.node_id).collect();
        assert!(
            assigned.iter().all(|&h| h == Some(0)),
            "expected chain colocated on host 0, got {:?}", assigned
        );
    }
}
