use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::placer::{assign_nodes, PlacementHints};
use crate::slot::{collect_slots, output_slot};
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
    // ── 0. Auto-assign node_ids for nodes that don't have one ─────────────────
    // Clone so we don't mutate the caller's SymbolicDag.
    let mut nodes: Vec<SymbolicNode> = dag.nodes.clone();
    let empty_hints = PlacementHints::default();
    assign_nodes(
        &mut nodes,
        dag.total_nodes,
        hints.unwrap_or(&empty_hints),
        dag.max_colocation,
    );

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
    if !dag.shared_inputs.is_empty() {
        out.insert(
            "shared_inputs".into(),
            serde_json::to_value(&dag.shared_inputs)?,
        );
    }
    out.insert("node_dags".into(), Value::Object(node_dags));

    Ok(Value::Object(out))
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
