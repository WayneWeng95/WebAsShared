use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use crate::placer::{assign_nodes, PlacementHints};
use crate::policies;
use crate::slot::{collect_slots, output_slot};
use crate::slot_assigner::assign_slots;
use crate::symbolic_dag::{SymbolicDag, SymbolicNode};

struct CrossEdge {
    src_machine: u32,
    src_slot: u32,
    dest_machine: u32,
    recv_slot: u32,
}

/// Partition a SymbolicDag into a ClusterDag-compatible JSON value.
///
/// `hints` provides optional per-host capacity weights (from `cluster_capacity`)
/// for auto-assigning nodes whose `node_id` is `None`.  Pass `None` to use
/// uniform distribution across all hosts.
///
/// The output has the same shape as a ClusterDag (shm_path_prefix, transfer,
/// node_dags, shared_inputs, …) and can be fed directly to
/// `dag_transform::transform_cluster_dag` and `ClusterDag::from_json`.
pub fn partition(dag: &SymbolicDag, hints: Option<&PlacementHints>) -> Result<Value> {
    // ── 0. Expand placement:"all" nodes, then fanout nodes ───────────────────
    let (after_all, auto_shared_inputs) = expand_all_placements(dag.nodes.clone(), dag.total_nodes);
    let mut nodes: Vec<SymbolicNode> = expand_fanout(after_all);
    // Hint priority: live hints (coordinator) > placement_policy > raw hints > default.
    let policy_hints: Option<PlacementHints> = dag.placement_policy.as_ref()
        .map(|p| policies::resolve_policy(p, dag.total_nodes));
    let default_hints = policies::default_hints(dag.total_nodes);
    let effective_hints = hints
        .or(policy_hints.as_ref())
        .or(dag.hints.as_ref())
        .unwrap_or(&default_hints);
    assign_nodes(
        &mut nodes,
        dag.total_nodes,
        effective_hints,
        dag.max_colocation,
    );

    // ── 0b. Slot assignment ───────────────────────────────────────────────────
    assign_slots(&mut nodes)?;

    // ── 1. Validate ──────────────────────────────────────────────────────────

    let id_set: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();

    for node in &nodes {
        let nid = node.node_id.ok_or_else(|| {
            anyhow!("node '{}' was not assigned a node_id (placer bug)", node.id)
        })?;
        if nid as usize >= dag.total_nodes {
            bail!(
                "node '{}' has node_id {} but total_nodes is {}",
                node.id, nid, dag.total_nodes
            );
        }
        for dep in &node.deps {
            if !id_set.contains(dep.as_str()) {
                bail!("node '{}' has unknown dep '{}'", node.id, dep);
            }
        }
    }

    // ── 2. Build id → node and id → output-slot maps ─────────────────────────

    let id_to_node: HashMap<&str, &SymbolicNode> =
        nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut id_to_slot: HashMap<&str, u32> = HashMap::new();
    for node in &nodes {
        let slot = node.output_slot.or_else(|| output_slot(&node.kind));
        if let Some(s) = slot {
            id_to_slot.insert(node.id.as_str(), s);
        }
    }

    // ── 3. Global slot scan → next free slot ─────────────────────────────────

    let mut all_slots: BTreeSet<u32> = BTreeSet::new();
    for node in &nodes {
        collect_slots(&node.kind, &mut all_slots);
    }
    let mut next_slot: u32 = all_slots.iter().last().copied().unwrap_or(0) + 1;

    // ── 4. Discover cross-node edges ─────────────────────────────────────────
    // key: (src_node_id, dest_machine_id)

    let mut cross_edges: HashMap<(String, u32), CrossEdge> = HashMap::new();

    for node in &nodes {
        let node_machine = node.node_id.unwrap(); // guaranteed by placer
        for dep_id in &node.deps {
            let dep_node = id_to_node[dep_id.as_str()];
            let dep_machine = dep_node.node_id.unwrap();
            if dep_machine == node_machine {
                continue; // same machine
            }

            let key = (dep_id.clone(), node_machine);
            if cross_edges.contains_key(&key) {
                continue;
            }

            // Input nodes distribute data via shared_inputs, not RemoteSend.
            if dep_node.kind.get("Input").is_some() {
                bail!(
                    "Input node '{}' cannot be a cross-node dep of '{}'; \
                     declare the file in shared_inputs instead",
                    dep_id, node.id
                );
            }

            let src_slot = id_to_slot.get(dep_id.as_str()).copied().ok_or_else(|| {
                anyhow!(
                    "node '{}' has a cross-node consumer '{}' but no detectable output slot; \
                     add an output_slot annotation to '{}'",
                    dep_id, node.id, dep_id
                )
            })?;

            let recv_slot = next_slot;
            next_slot += 1;

            cross_edges.insert(
                key,
                CrossEdge {
                    src_machine: dep_machine,
                    src_slot,
                    dest_machine: node_machine,
                    recv_slot,
                },
            );
        }
    }

    // ── 5. Build per-machine node lists ──────────────────────────────────────

    let mut machine_nodes: HashMap<u32, Vec<Value>> = (0..dag.total_nodes as u32)
        .map(|i| (i, Vec::new()))
        .collect();

    // 5a. RemoteRecv nodes — one per cross-edge, placed on dest_machine.
    //     Empty deps so they can start receiving as early as possible.
    for ((src_id, _dest), edge) in &cross_edges {
        let recv_id = format!("rr_{}_from_{}", src_id, edge.src_machine);
        let recv_node = json!({
            "id": recv_id,
            "deps": [],
            "kind": { "RemoteRecv": { "slot": edge.recv_slot, "slot_kind": "Stream", "peer": edge.src_machine } }
        });
        machine_nodes.get_mut(&edge.dest_machine).unwrap().push(recv_node);
    }

    // 5b. Original symbolic nodes — rewrite cross-node deps and resolve upstream_nodes.
    for sym_node in &nodes {
        let sym_machine = sym_node.node_id.unwrap();
        let rewritten_deps: Vec<String> = sym_node
            .deps
            .iter()
            .map(|dep_id| {
                let dep_node = id_to_node[dep_id.as_str()];
                if dep_node.node_id.unwrap() != sym_machine {
                    format!("rr_{}_from_{}", dep_id, dep_node.node_id.unwrap())
                } else {
                    dep_id.clone()
                }
            })
            .collect();

        let kind = resolve_upstream_nodes(
            &sym_node.kind,
            sym_machine,
            &id_to_slot,
            &cross_edges,
        )?;
        let kind = resolve_output_kind(&kind, &sym_node.deps, sym_machine, &cross_edges);
        let kind = translate_func_kind(kind);

        let mut obj = serde_json::Map::new();
        obj.insert("id".into(), json!(sym_node.id));
        obj.insert("deps".into(), json!(rewritten_deps));
        obj.insert("kind".into(), kind);
        if let Some(ref bg) = sym_node.barrier_group {
            obj.insert("barrier_group".into(), json!(bg));
        }

        machine_nodes
            .get_mut(&sym_machine)
            .unwrap()
            .push(Value::Object(obj));
    }

    // 5c. RemoteSend nodes — one per cross-edge, placed on src_machine.
    for ((src_id, _dest), edge) in &cross_edges {
        let send_id = format!("rs_{}_to_{}", src_id, edge.dest_machine);
        let send_node = json!({
            "id": send_id,
            "deps": [src_id],
            "kind": { "RemoteSend": { "slot": edge.src_slot, "slot_kind": "Stream", "peer": edge.dest_machine } }
        });
        machine_nodes
            .get_mut(&edge.src_machine)
            .unwrap()
            .push(send_node);
    }

    // ── 5d. Prevent wave-0 deadlock ──────────────────────────────────────────
    //
    // If machine M has both a RemoteRecv(from=P) and a RemoteSend(to=P), they
    // could both land in wave 0 (no deps), causing both machines to block
    // waiting for each other before the local pipeline has fired.
    //
    // Fix: add the RemoteSend as a dep of the RemoteRecv whenever doing so
    // doesn't introduce a cycle (i.e. recv is NOT already transitively reachable
    // from send — which would mean it's a true bidirectional exchange that must
    // run concurrently).
    for machine_node_list in machine_nodes.values_mut() {
        // Collect recv and send ids, keyed by peer.
        let mut recvs_by_peer: HashMap<u32, Vec<String>> = HashMap::new();
        let mut sends_by_peer: HashMap<u32, Vec<String>> = HashMap::new();
        for node in machine_node_list.iter() {
            let id = node["id"].as_str().unwrap_or("").to_string();
            if let Some(rr) = node["kind"].get("RemoteRecv").and_then(|v| v.as_object()) {
                if let Some(peer) = rr.get("peer").and_then(|v| v.as_u64()) {
                    recvs_by_peer.entry(peer as u32).or_default().push(id);
                }
            } else if let Some(rs) = node["kind"].get("RemoteSend").and_then(|v| v.as_object()) {
                if let Some(peer) = rs.get("peer").and_then(|v| v.as_u64()) {
                    sends_by_peer.entry(peer as u32).or_default().push(id);
                }
            }
        }

        if recvs_by_peer.is_empty() || sends_by_peer.is_empty() {
            continue;
        }

        // Build successor map: dep_id → [nodes that depend on dep_id].
        let mut succ_map: HashMap<String, Vec<String>> = HashMap::new();
        for node in machine_node_list.iter() {
            let id = node["id"].as_str().unwrap_or("").to_string();
            if let Some(deps) = node["deps"].as_array() {
                for dep in deps {
                    if let Some(d) = dep.as_str() {
                        succ_map.entry(d.to_string()).or_default().push(id.clone());
                    }
                }
            }
        }

        // For each peer that has both a recv and a send on this machine, check
        // whether adding send→dep of recv is safe (no cycle).
        let all_peers: Vec<u32> = recvs_by_peer.keys()
            .filter(|p| sends_by_peer.contains_key(p))
            .cloned()
            .collect();

        let mut deps_to_add: Vec<(String, String)> = Vec::new();
        for peer in all_peers {
            for recv_id in recvs_by_peer.get(&peer).unwrap() {
                for send_id in sends_by_peer.get(&peer).unwrap() {
                    // Adding send as dep of recv is safe only if send is NOT
                    // already reachable from recv — i.e. no forward path
                    // recv → … → send exists (which would become a cycle).
                    if !dag_reachable(&succ_map, recv_id, send_id) {
                        deps_to_add.push((recv_id.clone(), send_id.clone()));
                    }
                }
            }
        }

        for (recv_id, send_id) in &deps_to_add {
            for node in machine_node_list.iter_mut() {
                if node["id"].as_str() == Some(recv_id.as_str()) {
                    if let Some(deps) = node["deps"].as_array_mut() {
                        if !deps.iter().any(|d| d.as_str() == Some(send_id.as_str())) {
                            deps.push(json!(send_id));
                            eprintln!(
                                "[partitioner] ordered recv '{}' after send '{}' (deadlock prevention)",
                                recv_id, send_id
                            );
                        }
                    }
                }
            }
        }
    }

    // ── 6. Assemble output ───────────────────────────────────────────────────

    let node_dags: serde_json::Map<String, Value> = machine_nodes
        .into_iter()
        .map(|(id, nodes)| (id.to_string(), json!(nodes)))
        .collect();

    let mut out = serde_json::Map::new();
    out.insert("shm_path_prefix".into(), json!(dag.shm_path_prefix));
    if let Some(ref v) = dag.wasm_path {
        out.insert("wasm_path".into(), json!(v));
    }
    if let Some(ref v) = dag.python_script {
        out.insert("python_script".into(), json!(v));
    }
    if let Some(ref v) = dag.python_wasm {
        out.insert("python_wasm".into(), json!(v));
    }
    if let Some(ref v) = dag.log_level {
        out.insert("log_level".into(), json!(v));
    }
    if let Some(ref v) = dag.mode {
        out.insert("mode".into(), json!(v));
    }
    if let Some(v) = dag.runs {
        out.insert("runs".into(), json!(v));
    }
    out.insert("transfer".into(), json!(dag.transfer));
    // Merge explicitly declared shared_inputs with those auto-derived from placement:"all" Input nodes.
    let mut merged_si = dag.shared_inputs.clone();
    for si in auto_shared_inputs {
        if !merged_si.iter().any(|e| e.path == si.path) {
            merged_si.push(si);
        }
    }
    if !merged_si.is_empty() {
        out.insert("shared_inputs".into(), serde_json::to_value(&merged_si)?);
    }
    out.insert("node_dags".into(), Value::Object(node_dags));

    Ok(Value::Object(out))
}

/// Rewrite `Output { slot }` to use the RecvSlot when the dep is on another machine.
///
/// After `assign_slots`, Output nodes carry `slot: dep_output_slot`.  If that dep
/// ended up on a different machine, a RemoteRecv was injected and the data arrives
/// in `edge.recv_slot`, not the original dep slot.  This function patches the kind
/// so the executor reads from the right slot.
fn resolve_output_kind(
    kind: &Value,
    node_deps: &[String],
    machine_id: u32,
    cross_edges: &HashMap<(String, u32), CrossEdge>,
) -> Value {
    let Some(out_obj) = kind.get("Output").and_then(|v| v.as_object()) else {
        return kind.clone();
    };
    for dep_id in node_deps {
        if let Some(edge) = cross_edges.get(&(dep_id.clone(), machine_id)) {
            let mut new_out = out_obj.clone();
            new_out.insert("slot".into(), json!(edge.recv_slot));
            return json!({ "Output": Value::Object(new_out) });
        }
    }
    kind.clone()
}

/// Translate `{ "Func": { "func": F, "arg": A, "arg2": B } }` to
/// `{ "WasmVoid": { "func": F, "arg": A } }`.
///
/// `arg2` in the SymbolicDag is the output slot used only for cross-node
/// routing during partitioning. The executor does not know about it.
/// All other kind variants are returned unchanged.
fn translate_func_kind(kind: Value) -> Value {
    let Some(func_obj) = kind.get("Func").and_then(|v| v.as_object()) else {
        return kind;
    };
    let func = func_obj.get("func").cloned().unwrap_or(Value::Null);
    let arg  = func_obj.get("arg").cloned().unwrap_or(Value::Null);
    json!({ "WasmVoid": { "func": func, "arg": arg } })
}

/// Rewrite `upstream_nodes: ["A", "B"]` inside routing-node kind JSON to
/// `upstream: [<slot_A>, <slot_B>]` using concrete slot assignments.
///
/// For a symbolic node ref that lives on the same machine, the slot comes from
/// `id_to_slot`; for a cross-node ref it comes from the assigned recv_slot.
/// The `upstream_nodes` key is then removed from the kind object.
fn resolve_upstream_nodes(
    kind: &Value,
    machine_id: u32,
    id_to_slot: &HashMap<&str, u32>,
    cross_edges: &HashMap<(String, u32), CrossEdge>,
) -> Result<Value> {
    let obj = match kind.as_object() {
        Some(o) => o,
        None => return Ok(kind.clone()),
    };

    // Kind JSON has exactly one key: the variant name.
    if obj.len() != 1 {
        return Ok(kind.clone());
    }

    let (variant, inner) = obj.iter().next().unwrap();
    let inner_obj = match inner.as_object() {
        Some(o) => o,
        None => return Ok(kind.clone()),
    };

    if !inner_obj.contains_key("upstream_nodes") {
        return Ok(kind.clone());
    }

    let upstream_nodes = inner_obj["upstream_nodes"]
        .as_array()
        .ok_or_else(|| anyhow!("upstream_nodes must be a JSON array"))?;

    let mut resolved: Vec<u32> = Vec::new();
    for entry in upstream_nodes {
        let node_ref = entry
            .as_str()
            .ok_or_else(|| anyhow!("upstream_nodes entries must be strings"))?;

        let slot = if let Some(edge) =
            cross_edges.get(&(node_ref.to_string(), machine_id))
        {
            // Cross-node: use the RemoteRecv slot assigned for this edge.
            edge.recv_slot
        } else {
            // Same machine: look up the static output slot.
            *id_to_slot.get(node_ref).ok_or_else(|| {
                anyhow!(
                    "upstream_nodes ref '{}' has no detectable output slot; \
                     add an output_slot annotation to that node",
                    node_ref
                )
            })?
        };
        resolved.push(slot);
    }

    let mut new_inner = inner_obj.clone();
    new_inner.remove("upstream_nodes");
    new_inner.insert("upstream".into(), json!(resolved));

    let mut new_kind = serde_json::Map::new();
    new_kind.insert(variant.clone(), Value::Object(new_inner));
    Ok(Value::Object(new_kind))
}

/// BFS reachability check through a successor map (dep_id → dependents).
/// Returns `true` if `target` is reachable from `start` by following successors.
fn dag_reachable(succ_map: &HashMap<String, Vec<String>>, start: &str, target: &str) -> bool {
    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    queue.push_back(start);
    while let Some(node) = queue.pop_front() {
        if node == target {
            return true;
        }
        if !visited.insert(node) {
            continue;
        }
        if let Some(succs) = succ_map.get(node) {
            for s in succs {
                queue.push_back(s.as_str());
            }
        }
    }
    false
}

/// Expand nodes with `placement: "all"` into one copy per machine (`{id}_0` … `{id}_{N-1}`).
///
/// Dep-rewriting rules:
///   - An "all" copy M depending on another "all" template T → `T_M` (same-machine sibling).
///   - A regular node depending on an "all" template T → expanded to all N copies.
///
/// Returns the expanded node list and any `SharedInput` entries auto-derived from
/// `Input` nodes with `placement: "all"` (using source_node 0).
fn expand_all_placements(
    nodes: Vec<SymbolicNode>,
    total_nodes: usize,
) -> (Vec<SymbolicNode>, Vec<crate::symbolic_dag::SharedInput>) {
    use std::collections::HashSet;

    let all_templates: HashSet<String> = nodes
        .iter()
        .filter(|n| n.placement.as_deref() == Some("all"))
        .map(|n| n.id.clone())
        .collect();

    if all_templates.is_empty() {
        return (nodes, Vec::new());
    }

    let mut expanded: Vec<SymbolicNode> = Vec::new();
    let mut shared_inputs: Vec<crate::symbolic_dag::SharedInput> = Vec::new();

    for node in nodes {
        if node.placement.as_deref() == Some("all") {
            // Auto-derive shared_inputs from Input nodes so the coordinator stages
            // the file from machine 0 to all others before the DAG runs.
            if let Some(input_obj) = node.kind.get("Input").and_then(|v| v.as_object()) {
                if let Some(path) = input_obj.get("path").and_then(|v| v.as_str()) {
                    shared_inputs.push(crate::symbolic_dag::SharedInput {
                        path: path.to_string(),
                        source_node: 0,
                    });
                }
            }

            for machine in 0..total_nodes as u32 {
                let mut copy = node.clone();
                copy.id = format!("{}_{}", node.id, machine);
                copy.node_id = Some(machine);
                copy.placement = None;
                // Same-machine sibling rule for "all" → "all" deps.
                copy.deps = node
                    .deps
                    .iter()
                    .map(|dep| {
                        if all_templates.contains(dep) {
                            format!("{}_{}", dep, machine)
                        } else {
                            dep.clone()
                        }
                    })
                    .collect();
                expanded.push(copy);
            }
        } else {
            // Regular node: broadcast-expand any deps on "all" templates.
            let mut regular = node.clone();
            regular.deps = node
                .deps
                .iter()
                .flat_map(|dep| {
                    if all_templates.contains(dep) {
                        (0..total_nodes as u32)
                            .map(|m| format!("{}_{}", dep, m))
                            .collect::<Vec<_>>()
                    } else {
                        vec![dep.clone()]
                    }
                })
                .collect();
            expanded.push(regular);
        }
    }

    (expanded, shared_inputs)
}

/// Expand nodes with `fanout: N` into N identical copies named `{id}_0` … `{id}_{N-1}`.
/// Any other node listing the template id in its `deps` gets all N copies substituted in.
fn expand_fanout(nodes: Vec<SymbolicNode>) -> Vec<SymbolicNode> {
    // Map: template_id → list of expanded ids
    let mut expansions: HashMap<String, Vec<String>> = HashMap::new();
    let mut expanded: Vec<SymbolicNode> = Vec::new();

    for node in nodes {
        if let Some(n) = node.fanout {
            let ids: Vec<String> = (0..n).map(|i| format!("{}_{}", node.id, i)).collect();
            expansions.insert(node.id.clone(), ids.clone());
            for id in ids {
                let mut copy = node.clone();
                copy.id = id;
                copy.fanout = None;
                expanded.push(copy);
            }
        } else {
            expanded.push(node);
        }
    }

    // Rewrite deps: replace each template_id with all its expanded ids
    for node in &mut expanded {
        let mut new_deps: Vec<String> = Vec::new();
        for dep in &node.deps {
            if let Some(ids) = expansions.get(dep) {
                new_deps.extend_from_slice(ids);
            } else {
                new_deps.push(dep.clone());
            }
        }
        node.deps = new_deps;
    }

    expanded
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbolic_dag::SymbolicDag;

    const WORD_COUNT_SYMBOLIC: &str = include_str!("../tests/word_count_symbolic.json");

    #[test]
    fn partition_word_count() {
        let dag = SymbolicDag::from_json(WORD_COUNT_SYMBOLIC).expect("parse");
        let out = partition(&dag, None).expect("partition");

        // Both machines must be present.
        let node_dags = out["node_dags"].as_object().unwrap();
        assert!(node_dags.contains_key("0"), "missing node_dags[\"0\"]");
        assert!(node_dags.contains_key("1"), "missing node_dags[\"1\"]");

        let n0: Vec<Value> = serde_json::from_value(node_dags["0"].clone()).unwrap();
        let n1: Vec<Value> = serde_json::from_value(node_dags["1"].clone()).unwrap();

        // Node 0 must contain a RemoteSend for aggregate → machine 1.
        let has_send = n0.iter().any(|n| {
            n["id"].as_str() == Some("rs_aggregate_to_1")
                && n["kind"].get("RemoteSend").is_some()
        });
        assert!(has_send, "missing rs_aggregate_to_1 in node 0: {:?}", n0);

        // Node 1 must contain the matching RemoteRecv.
        let recv_node = n1
            .iter()
            .find(|n| n["id"].as_str() == Some("rr_aggregate_from_0"))
            .expect("missing rr_aggregate_from_0 in node 1");
        assert!(recv_node["kind"].get("RemoteRecv").is_some());

        // The recv slot must be greater than all slots that appeared in the input
        // (i.e., freshly allocated, not conflicting with 0, 10-19, 110-119, 200, 300).
        let recv_slot = recv_node["kind"]["RemoteRecv"]["slot"]
            .as_u64()
            .expect("recv slot") as u32;
        assert!(recv_slot > 300, "recv_slot should be > 300, got {}", recv_slot);

        // aggregate_global on node 1 must have upstream: [recv_slot, 200].
        let global = n1
            .iter()
            .find(|n| n["id"].as_str() == Some("aggregate_global"))
            .expect("missing aggregate_global in node 1");
        let upstream = global["kind"]["Aggregate"]["upstream"]
            .as_array()
            .expect("upstream array");
        let slots: Vec<u32> = upstream.iter().map(|v| v.as_u64().unwrap() as u32).collect();
        assert!(slots.contains(&recv_slot), "upstream missing recv_slot");
        assert!(slots.contains(&200), "upstream missing 200");
    }
}
