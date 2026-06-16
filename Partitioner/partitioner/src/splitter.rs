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

/// Pin the convergence tail onto `coordinator_id` (the input/distributor node).
///
/// Starting from every `Output` node, walk backward through `deps`, pinning each
/// auto-placed (`node_id == None`) node to `coordinator_id`.  Recursion follows
/// only single-dep (linear-tail) nodes; at the first fan-in node (≥2 deps — the
/// convergence point) the node is pinned but its branches are NOT followed, so
/// the upstream parallel work the placer would distribute is left alone.
///
/// Net effect on a typical map-reduce DAG: `aggregate_global`, `reduce`, and
/// `save` land on node 0; the per-machine maps/aggregates stay where placed.
fn pin_converge_to_coordinator(nodes: &mut [SymbolicNode], coordinator_id: u32) {
    let id_to_idx: HashMap<String, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.id.clone(), i)).collect();
    let mut stack: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.kind.get("Output").is_some())
        .map(|(i, _)| i)
        .collect();
    let mut visited: HashSet<usize> = HashSet::new();
    while let Some(idx) = stack.pop() {
        if !visited.insert(idx) {
            continue;
        }
        if nodes[idx].node_id.is_none() {
            nodes[idx].node_id = Some(coordinator_id);
        }
        // Follow only a single-dep linear tail; stop at the first fan-in so we
        // don't pull the distributed parallel branches onto the coordinator.
        if nodes[idx].deps.len() == 1 {
            if let Some(&d) = id_to_idx.get(&nodes[idx].deps[0]) {
                stack.push(d);
            }
        }
    }
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
/// Auto-reclaim fan-out worker input slots once their consumers have run.
///
/// A fan-out `Func` (one declaring `out_base`, e.g. `wc_distribute`) zero-copy
/// **relinks** the input pages into per-worker stream slots that its consumer
/// `Func`s (e.g. `wc_map`) read.  Those pages are dead after the consumers run,
/// but the host auto-reclaimer can't see generic `Func` slot semantics, so they
/// would sit in SHM for the rest of the DAG.  For every fan-out source we emit a
/// `FreeSlots` node (one per machine) that depends on the **same-machine**
/// consumers and frees the stream slots they read.  Cross-machine consumers are
/// fed via `RemoteRecv`, whose produced slots are already reclaimed when the
/// consumer finishes — so we skip those to avoid double-freeing.
///
/// Runs after `assign_slots`, when each fan-out source still carries `out_base`
/// and each consumer's input slot is in its `Func.arg`.
fn inject_fanout_frees(nodes: &mut Vec<SymbolicNode>) {
    // fan-out source id → its machine
    let fanout_src: HashMap<String, u32> = nodes
        .iter()
        .filter(|n| n.kind.get("Func").and_then(|f| f.get("out_base")).is_some())
        .filter_map(|n| n.node_id.map(|m| (n.id.clone(), m)))
        .collect();
    if fanout_src.is_empty() {
        return;
    }

    // (fanout_src_id, machine) → (consumer ids, stream slots to free)
    let mut groups: HashMap<(String, u32), (Vec<String>, Vec<u32>)> = HashMap::new();
    for n in nodes.iter() {
        let Some(func) = n.kind.get("Func").and_then(|v| v.as_object()) else { continue };
        // A consumer reads a fan-out worker slot via `output_offset` (input = arg).
        if !func.contains_key("output_offset") {
            continue;
        }
        let (Some(in_slot), Some(m)) =
            (func.get("arg").and_then(|v| v.as_u64()), n.node_id) else { continue };
        for dep in &n.deps {
            if fanout_src.get(dep) == Some(&m) {
                let e = groups.entry((dep.clone(), m)).or_default();
                e.0.push(n.id.clone());
                e.1.push(in_slot as u32);
            }
        }
    }

    let mut new_nodes: Vec<SymbolicNode> = groups
        .into_iter()
        .map(|((src_id, machine), (consumers, mut slots))| {
            slots.sort_unstable();
            slots.dedup();
            SymbolicNode {
                id: format!("free_{src_id}_m{machine}"),
                deps: consumers,
                node_id: Some(machine),
                output_slot: None,
                fanout: None,
                placement: None,
                barrier_group: None,
                split: None,
                out_weight: None,
                kind: json!({ "FreeSlots": { "stream": slots } }),
            }
        })
        .collect();
    new_nodes.sort_by(|a, b| a.id.cmp(&b.id)); // deterministic output
    nodes.append(&mut new_nodes);
}

pub fn partition(dag: &SymbolicDag, hints: Option<&PlacementHints>) -> Result<Value> {
    // total_nodes is optional in the DAG: the coordinator sets it from the live
    // cluster size before partitioning; the standalone CLI defaults it.  Resolve
    // a concrete count here (≥1) and use it throughout.
    let total_nodes = dag.total_nodes.unwrap_or(1).max(1);
    // Hint priority: live hints (coordinator) > placement_policy > raw hints > default.
    // Resolved BEFORE expansion because capacity-aware fanout apportionment
    // (expand_all_placements) needs the per-host capacity weights.
    let policy_hints: Option<PlacementHints> = dag.placement_policy.as_ref()
        .map(|p| policies::resolve_policy(p, total_nodes));
    let default_hints = policies::default_hints(total_nodes);
    let effective_hints = hints
        .or(policy_hints.as_ref())
        .or(dag.hints.as_ref())
        .unwrap_or(&default_hints);

    // ── 0. Expand placement:"all" nodes (capacity-aware fanout), then fanout ──
    // A placement:"all" node with `fanout: N` splits N TOTAL workers across the
    // cluster in proportion to capacity (so a node with 80% capacity gets 80% of
    // the workers — and, via assign_input_slices, 80% of the input slice).
    let (after_all, auto_shared_inputs) =
        expand_all_placements(dag.nodes.clone(), total_nodes, effective_hints);
    let (nodes0, fanout_worker_ids): (Vec<SymbolicNode>, HashSet<String>) = expand_fanout(after_all);

    // ── 0·. Cross-host streaming: auto-split any StreamPipeline marked `cut_after`
    // into two RDMA-wired halves + a StreamOutput return sink (pre-pinned, so the
    // placer below leaves them be).  No-op when no pipeline carries `cut_after`.
    let mut nodes = split_stream_pipelines(nodes0, total_nodes, effective_hints)?;

    // ── 0a. Converge-on-coordinator: before auto-placement, pin the convergence
    // tail (Output + its single-chain auto ancestors up to the first fan-in)
    // onto node 0, so the final reduce/output co-locate with the node-0 input.
    // Gated by the DAG override or the compile-time default; respects explicit
    // pins and per-machine ("all") nodes (they are already `Some`).
    let converge = dag
        .converge_on_coordinator
        .unwrap_or(node_agent_common::PARTITIONER_CONVERGE_ON_COORDINATOR);
    if converge {
        pin_converge_to_coordinator(&mut nodes, 0);
    }

    assign_nodes(
        &mut nodes,
        total_nodes,
        effective_hints,
        dag.max_colocation,
    );

    // ── 0a'. Data-parallel input sharding ─────────────────────────────────────
    // Now that every node has a final machine, stamp each replicated `load`
    // Input with a line-aligned fractional slice weighted by the map workers on
    // its machine, so the N nodes process disjoint shards (result == 1×, not N×).
    let shared_paths: HashSet<String> =
        auto_shared_inputs.iter().map(|s| s.path.clone()).collect();
    assign_input_slices(&mut nodes, total_nodes, &fanout_worker_ids, &shared_paths);

    // ── 0b. Slot assignment ───────────────────────────────────────────────────
    assign_slots(&mut nodes)?;

    // ── 0c. Auto-reclaim fan-out worker input slots after their consumers run ──
    inject_fanout_frees(&mut nodes);

    // ── 1. Validate ──────────────────────────────────────────────────────────

    let id_set: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();

    for node in &nodes {
        let nid = node.node_id.ok_or_else(|| {
            anyhow!("node '{}' was not assigned a node_id (placer bug)", node.id)
        })?;
        if nid as usize >= total_nodes {
            bail!(
                "node '{}' has node_id {} but total_nodes is {}",
                node.id, nid, total_nodes
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

    let mut machine_nodes: HashMap<u32, Vec<Value>> = (0..total_nodes as u32)
        .map(|i| (i, Vec::new()))
        .collect();

    // 5a. RemoteRecv nodes — one per cross-edge, placed on dest_machine.
    //     Empty deps so they can start receiving as early as possible.
    for ((src_id, _dest), edge) in &cross_edges {
        let recv_id = format!("rr_{}_from_{}", src_id, edge.src_machine);
        let recv_node = json!({
            "id": recv_id,
            "deps": [],
            "kind": { "RemoteRecv": { "slot": edge.recv_slot, "slot_kind": cross_edge_slot_kind(edge.src_slot), "peer": edge.src_machine } }
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
        // For a fan-out Func (has `out_base`), the guest needs the consumer
        // count alongside the base slot.  Count this node's direct consumers and
        // let translate_func_kind pack it into the high bits of `arg`.
        let n_consumers = if sym_node.kind.get("Func").and_then(|f| f.get("out_base")).is_some() {
            nodes.iter().filter(|m| m.deps.iter().any(|d| d == &sym_node.id)).count() as u32
        } else {
            0
        };
        let kind = translate_func_kind(kind, n_consumers);

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
            "kind": { "RemoteSend": { "slot": edge.src_slot, "slot_kind": cross_edge_slot_kind(edge.src_slot), "peer": edge.dest_machine } }
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
            } else if let Some(sp) = node["kind"].get("StreamPipeline").and_then(|v| v.as_object()) {
                // A StreamPipeline's embedded rdma_send drives the remote side; an
                // output-return RemoteRecv on this machine must be sequenced AFTER it
                // (otherwise the recv blocks wave 0 before the source pipeline that
                // feeds the remote computation ever runs → cross-node deadlock).
                if let Some(peer) = sp.get("rdma_send")
                    .and_then(|s| s.get("peer")).and_then(|v| v.as_u64())
                {
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
        // whether adding send→dep of recv is safe (no cycle). Sort peers so the
        // outcome is deterministic regardless of HashMap iteration order.
        let mut all_peers: Vec<u32> = recvs_by_peer.keys()
            .filter(|p| sends_by_peer.contains_key(p))
            .cloned()
            .collect();
        all_peers.sort_unstable();

        let mut deps_to_add: Vec<(String, String)> = Vec::new();
        for peer in all_peers {
            for recv_id in recvs_by_peer.get(&peer).unwrap() {
                for send_id in sends_by_peer.get(&peer).unwrap() {
                    // Adding send as dep of recv is safe only if send is NOT
                    // already reachable from recv — i.e. no forward path
                    // recv → … → send exists (which would become a cycle).
                    //
                    // Crucially, check against the LIVE succ_map, which includes
                    // edges committed earlier in this same pass: two send→recv
                    // edges that are each individually safe against the original
                    // graph can still combine to close a cycle (e.g. two cross-
                    // node exchanges that chain recv→…→send through the other's
                    // newly-added edge). Updating succ_map as we go makes each
                    // later candidate see the edges already added.
                    if !dag_reachable(&succ_map, recv_id, send_id) {
                        deps_to_add.push((recv_id.clone(), send_id.clone()));
                        // New edge: send_id → recv_id (recv now depends on send).
                        succ_map.entry(send_id.clone()).or_default().push(recv_id.clone());
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

/// Slot kind for an auto-injected cross-edge RemoteSend/RemoteRecv.
///
/// The slot model reserves slot 0 (INPUT_IO_SLOT) and slot 1 (OUTPUT_IO_SLOT) as
/// I/O slots and allocates every stream slot at ≥2. A cross-edge whose source is
/// the terminal OUTPUT_IO_SLOT is an *output return* (e.g. a worker's processed
/// records flowing back to the coordinator's `Output`), which lives in the I/O
/// record store (`io_heads`, length-prefixed records read by `read_io_records` /
/// `Output { split_records }`). Every other cross-edge carries an intermediate
/// stream slot. `Input` nodes are never cross-edge sources (the splitter rejects
/// that), so OUTPUT_IO_SLOT is the only I/O slot that can appear here.
fn cross_edge_slot_kind(src_slot: u32) -> &'static str {
    const OUTPUT_IO_SLOT: u32 = 1;
    if src_slot == OUTPUT_IO_SLOT { "Io" } else { "Stream" }
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
///
/// For a fan-out Func (one declaring `out_base`), `n_consumers` is packed into
/// the high 16 bits of `arg` (base in the low 16): the guest reads the base
/// slot to write from and the worker count to know how many contiguous slots to
/// fill.  Packing here — rather than at slot-assignment time — keeps the bare
/// base slot visible to the partitioner's slot bookkeeping (`collect_slots`).
///
/// All other kind variants are returned unchanged.
fn translate_func_kind(kind: Value, n_consumers: u32) -> Value {
    let Some(func_obj) = kind.get("Func").and_then(|v| v.as_object()) else {
        return kind;
    };
    let func = func_obj.get("func").cloned().unwrap_or(Value::Null);
    let arg_val = func_obj.get("arg").cloned().unwrap_or(Value::Null);

    // Fan-out node: pack (base | n_consumers << 16) so the guest gets both.
    if func_obj.contains_key("out_base") && n_consumers > 0 {
        if let Some(base) = arg_val.as_u64() {
            let packed = (base & 0xFFFF) | ((n_consumers as u64) << 16);
            return json!({ "WasmVoid": { "func": func, "arg": packed } });
        }
    }
    json!({ "WasmVoid": { "func": func, "arg": arg_val } })
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
/// Clamp a requested `fanout` to the cluster's usable core budget:
/// `Σ_hosts max(1, cores_host − reserve)`, reserving
/// `PARTITIONER_FANOUT_CORES_RESERVED_PER_HOST` cores per machine for the
/// control plane.  Prevents an over-large `fanout` (e.g. 50 on a 2×16-core
/// cluster) from spawning more workers than the cluster can usefully run.
///
/// Returns `n` unchanged when no per-host core data is available (e.g. the
/// standalone CLI with no hints), so non-cluster partitioning is unaffected.
fn cap_fanout(n: usize, total_nodes: usize, hints: &PlacementHints) -> usize {
    if hints.cores.is_empty() {
        return n;
    }
    let reserve = node_agent_common::PARTITIONER_FANOUT_CORES_RESERVED_PER_HOST;
    let budget: usize = (0..total_nodes as u32)
        .filter_map(|m| hints.cores.get(&m))
        .map(|&c| c.saturating_sub(reserve).max(1))
        .sum();
    if budget == 0 { n } else { n.min(budget) }
}

/// Split `n` total fanout copies across `total_nodes` machines in proportion to
/// per-host capacity, using the largest-remainder (Hamilton) method so the parts
/// sum to EXACTLY `n`.  Empty/zero capacity → uniform split.  A host with
/// negligible capacity may receive 0 (it then loads an empty input slice and
/// contributes nothing — handled downstream by the empty-aggregate / 0-byte
/// RemoteSend paths).
fn apportion_fanout(n: usize, total_nodes: usize, hints: &PlacementHints) -> Vec<usize> {
    if total_nodes <= 1 {
        return vec![n; total_nodes];
    }
    let mut caps: Vec<f64> = (0..total_nodes as u32)
        .map(|m| hints.capacity.get(&m).copied().unwrap_or(0.0))
        .collect();
    if caps.iter().sum::<f64>() <= 0.0 {
        caps = vec![1.0; total_nodes]; // no capacity data → uniform
    }
    let sum: f64 = caps.iter().sum();
    let raw: Vec<f64> = caps.iter().map(|c| c / sum * n as f64).collect();
    let mut counts: Vec<usize> = raw.iter().map(|r| r.floor() as usize).collect();
    let mut remainder = n - counts.iter().sum::<usize>();
    // Hand the leftover (from flooring) to the largest fractional parts.
    let mut order: Vec<usize> = (0..total_nodes).collect();
    order.sort_by(|&a, &b| {
        let fa = raw[a] - raw[a].floor();
        let fb = raw[b] - raw[b].floor();
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    for &i in &order {
        if remainder == 0 { break; }
        counts[i] += 1;
        remainder -= 1;
    }
    counts
}

fn expand_all_placements(
    nodes: Vec<SymbolicNode>,
    total_nodes: usize,
    hints: &PlacementHints,
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

            // Capacity-aware fanout: first clamp the requested N to the cluster
            // core budget, then split it across machines by capacity (Hamilton).
            // Non-fanout "all" nodes are unaffected.
            let per_machine_fanout = node.fanout.map(|n| {
                let capped = cap_fanout(n, total_nodes, hints);
                if capped < n {
                    eprintln!(
                        "[partitioner] fanout for '{}' capped {} → {} (cluster core budget, {} reserved/host)",
                        node.id, n, capped,
                        node_agent_common::PARTITIONER_FANOUT_CORES_RESERVED_PER_HOST,
                    );
                }
                apportion_fanout(capped, total_nodes, hints)
            });

            for machine in 0..total_nodes as u32 {
                let mut copy = node.clone();
                copy.id = format!("{}_{}", node.id, machine);
                copy.node_id = Some(machine);
                copy.placement = None;
                if let Some(ref counts) = per_machine_fanout {
                    copy.fanout = Some(counts[machine as usize]);
                }
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
///
/// Returns the expanded node list plus the set of ids that are fanout worker
/// copies — used to weight each machine's data-parallel input slice by the
/// number of map workers placed on it (see `assign_input_slices`).
fn expand_fanout(nodes: Vec<SymbolicNode>) -> (Vec<SymbolicNode>, HashSet<String>) {
    // Map: template_id → list of expanded ids
    let mut expansions: HashMap<String, Vec<String>> = HashMap::new();
    let mut expanded: Vec<SymbolicNode> = Vec::new();
    let mut worker_ids: HashSet<String> = HashSet::new();

    for node in nodes {
        if let Some(n) = node.fanout {
            let ids: Vec<String> = (0..n).map(|i| format!("{}_{}", node.id, i)).collect();
            expansions.insert(node.id.clone(), ids.clone());
            for id in ids {
                let mut copy = node.clone();
                copy.id = id.clone();
                copy.fanout = None;
                worker_ids.insert(id);
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

    (expanded, worker_ids)
}

// ── Phase 2: cross-host streaming — auto-split a StreamPipeline at `cut_after` ──

/// Build a `SymbolicNode` with an explicit machine pin and constructed kind.
fn pinned_node(id: String, deps: Vec<String>, node_id: u32, kind: Value) -> SymbolicNode {
    SymbolicNode {
        id, deps, node_id: Some(node_id),
        output_slot: None, fanout: None, placement: None,
        barrier_group: None, split: None, out_weight: None, kind,
    }
}

/// Assemble a `StreamPipeline` kind from a stage slice + optional RDMA config.
fn stream_pipeline_kind(rounds: u64, stages: Vec<Value>,
                        rdma_recv: Option<Value>, rdma_send: Option<Value>) -> Value {
    let mut sp = serde_json::Map::new();
    sp.insert("rounds".into(), json!(rounds));
    sp.insert("stages".into(), Value::Array(stages));
    if let Some(r) = rdma_recv { sp.insert("rdma_recv".into(), r); }
    if let Some(s) = rdma_send { sp.insert("rdma_send".into(), s); }
    json!({ "StreamPipeline": Value::Object(sp) })
}

/// Cost of cutting after a stage = the data it emits per round (a relative hint):
/// `split:"avoid"` → ∞ (never cut here), `split:"prefer"` → 0 (cut here first),
/// else `out_weight` (default 1.0).
fn boundary_cost(stage: &Value) -> f64 {
    match stage.get("split").and_then(|v| v.as_str()) {
        Some("avoid")  => f64::INFINITY,
        Some("prefer") => 0.0,
        _ => stage.get("out_weight").and_then(|v| v.as_f64()).unwrap_or(1.0),
    }
}

/// Pick `m-1` cut points (after-stage indices in `[0, len-2]`) for `m` segments,
/// cheapest first (skipping `avoid`=∞), tie-broken by earliest index.  Sorted asc.
fn choose_cuts(stages: &[Value], m: usize) -> Vec<usize> {
    if m <= 1 || stages.len() < 2 { return Vec::new(); }
    let mut cand: Vec<(usize, f64)> = (0..stages.len() - 1)
        .map(|k| (k, boundary_cost(&stages[k])))
        .filter(|&(_, c)| c.is_finite())
        .collect();
    cand.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    let mut cuts: Vec<usize> = cand.into_iter().take(m - 1).map(|(k, _)| k).collect();
    cuts.sort_unstable();
    cuts
}

/// Segment placement order: coordinator (0) first, then the other machines by the
/// capacity hint (desc).  On a homogeneous cluster this is `0,1,2,…`.
fn machine_order(total_nodes: usize, hints: &PlacementHints) -> Vec<u32> {
    let mut rest: Vec<u32> = (1..total_nodes as u32).collect();
    rest.sort_by(|a, b| {
        let (ca, cb) = (hints.capacity.get(a).copied().unwrap_or(0.0),
                        hints.capacity.get(b).copied().unwrap_or(0.0));
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(b))
    });
    std::iter::once(0u32).chain(rest).collect()
}

/// Resolve a pipeline's cut points: explicit `cut_after` (int or list) wins; else
/// `segments: M` auto-picks the M-1 cheapest boundaries.  Returns validated, sorted
/// cut indices, or `None` when the pipeline should not be split.
fn cut_points_for(sp: &serde_json::Map<String, Value>, total_nodes: usize, who: &str)
    -> Result<Option<Vec<usize>>>
{
    let stages = match sp.get("stages").and_then(|v| v.as_array()) {
        Some(s) => s, None => return Ok(None),
    };
    let mut cuts: Vec<usize> = if let Some(ca) = sp.get("cut_after") {
        match ca {
            Value::Number(_) => ca.as_u64().map(|k| vec![k as usize]).unwrap_or_default(),
            Value::Array(a)  => a.iter().filter_map(|v| v.as_u64().map(|x| x as usize)).collect(),
            _ => bail!("'{}' cut_after must be an integer or a list of integers", who),
        }
    } else if let Some(m) = sp.get("segments").and_then(|v| v.as_u64()).map(|m| m as usize) {
        if m <= 1 { return Ok(None); }
        choose_cuts(stages, m.min(total_nodes).min(stages.len()))
    } else {
        return Ok(None);
    };
    cuts.sort_unstable();
    cuts.dedup();
    if cuts.is_empty() { return Ok(None); }
    if *cuts.last().unwrap() + 1 >= stages.len() {
        bail!("'{}' cut after stage {} leaves no stages downstream ({} stages)",
              who, cuts.last().unwrap(), stages.len());
    }
    if cuts.len() + 1 > total_nodes {
        bail!("'{}' needs {} machines (segments) but total_nodes is {}",
              who, cuts.len() + 1, total_nodes);
    }
    Ok(Some(cuts))
}

/// Auto-split any `StreamPipeline` carrying `segments: M` (or explicit `cut_after`)
/// into M cross-node streaming SEGMENTS.  Cut points fall where the data is cheapest
/// (per-stage `split`/`out_weight`); segments are placed on machines ordered by the
/// capacity hint (coordinator first).  Each segment is a `StreamPipeline` wired to its
/// neighbours by embedded rdma_send/rdma_recv (streaming lane); the last segment
/// returns each round to node 0 (Io), where the terminal `Output` becomes a
/// `StreamOutput` sink.  No-op when `total_nodes < 2` or no pipeline is marked.
fn split_stream_pipelines(nodes: Vec<SymbolicNode>, total_nodes: usize, hints: &PlacementHints)
    -> Result<Vec<SymbolicNode>>
{
    let mut to_split: HashMap<String, Vec<usize>> = HashMap::new();
    for n in &nodes {
        if let Some(sp) = n.kind.get("StreamPipeline").and_then(|v| v.as_object()) {
            if let Some(cuts) = cut_points_for(sp, total_nodes, &n.id)? {
                to_split.insert(n.id.clone(), cuts);
            }
        }
    }
    if to_split.is_empty() { return Ok(nodes); }
    if total_nodes < 2 {
        bail!("StreamPipeline cut needs total_nodes ≥ 2 (got {})", total_nodes);
    }
    let machines = machine_order(total_nodes, hints);

    let mut out: Vec<SymbolicNode> = Vec::with_capacity(nodes.len() + 2);
    // pipe id → (return_slot, last_machine, seg0_deps) for the sink conversion.
    let mut split_meta: HashMap<String, (u32, u32, Vec<String>)> = HashMap::new();

    // First pass: emit the M segments per split pipeline.
    for node in &nodes {
        let Some(cuts) = to_split.get(&node.id) else { continue };
        let sp = node.kind.get("StreamPipeline").and_then(|v| v.as_object()).unwrap();
        let rounds = sp.get("rounds").and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("'{}' StreamPipeline missing rounds", node.id))?;
        let stages = sp.get("stages").and_then(|v| v.as_array()).unwrap();
        let m = cuts.len() + 1; // segment count

        // Stage range [lo, hi) of each segment.
        let mut bounds: Vec<(usize, usize)> = Vec::with_capacity(m);
        let mut lo = 0usize;
        for &c in cuts { bounds.push((lo, c + 1)); lo = c + 1; }
        bounds.push((lo, stages.len()));

        let boundary_slot = |c: usize| -> Result<u32> {
            stages[c].get("arg1").and_then(|v| v.as_u64()).map(|v| v as u32)
                .ok_or_else(|| anyhow!("'{}' stage {} has no arg1 to cut on", node.id, c))
        };
        let return_slot = stages.last().and_then(|s| s.get("arg1")).and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("'{}' last stage has no arg1 for the return", node.id))? as u32;

        for s in 0..m {
            let (slo, shi) = bounds[s];
            let seg_stages: Vec<Value> = stages[slo..shi].to_vec();
            let recv = if s > 0 {
                Some(json!({ "peer": machines[s - 1], "slot": boundary_slot(cuts[s - 1])?, "slot_kind": "Stream" }))
            } else { None };
            let send = if s < m - 1 {
                Some(json!({ "peer": machines[s + 1], "slot": boundary_slot(cuts[s])?, "slot_kind": "Stream", "free_after": true }))
            } else {
                Some(json!({ "peer": 0, "slot": return_slot, "slot_kind": "Io", "free_after": true }))
            };
            let deps = if s == 0 { node.deps.clone() } else { Vec::new() };
            out.push(pinned_node(format!("{}__s{}", node.id, s), deps, machines[s],
                                 stream_pipeline_kind(rounds, seg_stages, recv, send)));
        }
        split_meta.insert(node.id.clone(), (return_slot, machines[m - 1], node.deps.clone()));
    }

    // Second pass: convert each split pipeline's terminal Output → StreamOutput sink;
    // drop the original pipeline nodes; pass everything else through.
    for node in nodes {
        if to_split.contains_key(&node.id) { continue; }
        let split_dep = node.deps.iter().find(|d| split_meta.contains_key(*d)).cloned();
        if let (Some(pipe_id), Some(out_obj)) =
            (split_dep, node.kind.get("Output").and_then(|v| v.as_object()))
        {
            let (return_slot, last_machine, seg0_deps) = split_meta[&pipe_id].clone();
            let paths = out_obj.get("paths").and_then(|v| v.as_array()).cloned()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| anyhow!(
                    "Output '{}' returning a split StreamPipeline must use `paths` \
                     (one per round) for the StreamOutput sink", node.id))?;
            let rounds = out.iter().find(|n| n.id == format!("{}__s0", pipe_id))
                .and_then(|n| n.kind.get("StreamPipeline"))
                .and_then(|sp| sp.get("rounds")).and_then(|v| v.as_u64()).unwrap_or(0);
            // Sink shares the source segment's wave (deps = the pipeline's original
            // deps), runs as a thread, and pulls each round back over RDMA from the
            // last segment's machine.
            let sink_kind = json!({ "StreamOutput": {
                "rounds": rounds, "slot": return_slot, "slot_kind": "Io", "binary": true,
                "rdma_recv": { "peer": last_machine, "slot": return_slot, "slot_kind": "Io" },
                "paths": Value::Array(paths),
            }});
            out.push(pinned_node(node.id.clone(), seg0_deps, 0, sink_kind));
            continue;
        }
        out.push(node);
    }

    Ok(out)
}

/// Stamp each per-machine data-parallel `Input` replica with a line-aligned
/// fractional `slice: [lo, hi]` so the N nodes together cover the file exactly
/// once (result == single-node, not N×) and each node's SHM holds only its
/// shard.  The slice width on machine `m` is proportional to the number of
/// fanout map workers placed there: `weight_m = maps_on_m / total_maps`; the
/// host resolves the fractions to byte offsets against the actual file at load
/// time.  Only Inputs whose file is replicated to every node (`shared_paths`,
/// i.e. `placement:"all"` inputs) are sliced; single-machine Inputs load whole.
fn assign_input_slices(
    nodes: &mut [SymbolicNode],
    total_nodes: usize,
    worker_ids: &HashSet<String>,
    shared_paths: &HashSet<String>,
) {
    if total_nodes <= 1 || shared_paths.is_empty() {
        return; // single node (or nothing replicated): load the whole file.
    }

    // Per-machine map-worker counts → weights.  Fall back to equal weights when
    // the DAG has no fanout workers at all (uniform split).
    let mut weights = vec![0.0f64; total_nodes];
    for node in nodes.iter() {
        if let (Some(m), true) = (node.node_id, worker_ids.contains(&node.id)) {
            if (m as usize) < total_nodes {
                weights[m as usize] += 1.0;
            }
        }
    }
    if weights.iter().all(|&w| w == 0.0) {
        weights.iter_mut().for_each(|w| *w = 1.0);
    }
    let total_w: f64 = weights.iter().sum();
    if total_w == 0.0 {
        return;
    }

    // Cumulative fraction boundaries per machine: machine m owns [pre/W, (pre+w)/W).
    let mut prefix = vec![0.0f64; total_nodes + 1];
    for m in 0..total_nodes {
        prefix[m + 1] = prefix[m] + weights[m];
    }

    for node in nodes.iter_mut() {
        let Some(m) = node.node_id else { continue };
        let m = m as usize;
        let Some(input_obj) = node.kind.get("Input").and_then(|v| v.as_object()) else { continue };
        let is_shared = input_obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| shared_paths.contains(p))
            .unwrap_or(false);
        // Don't slice chunked/binary inputs — those have their own loading paths.
        let special = input_obj.contains_key("chunk_bytes") || input_obj.contains_key("binary");
        if !is_shared || special || m >= total_nodes {
            continue;
        }
        let lo = prefix[m] / total_w;
        let hi = prefix[m + 1] / total_w;
        let mut new_in = input_obj.clone();
        new_in.insert("slice".to_string(), json!([lo, hi]));
        node.kind = json!({ "Input": Value::Object(new_in) });
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbolic_dag::SymbolicDag;

    const WORD_COUNT_SYMBOLIC: &str = include_str!("../tests/word_count_symbolic.json");

    fn input_node(id: &str, machine: u32, path: &str) -> SymbolicNode {
        SymbolicNode {
            id: id.to_string(), deps: vec![], node_id: Some(machine),
            output_slot: None, fanout: None, placement: None, barrier_group: None, split: None, out_weight: None,
            kind: json!({ "Input": { "path": path } }),
        }
    }
    fn map_node(id: &str, machine: u32) -> SymbolicNode {
        SymbolicNode {
            id: id.to_string(), deps: vec![], node_id: Some(machine),
            output_slot: None, fanout: None, placement: None, barrier_group: None, split: None, out_weight: None,
            kind: json!({ "WasmVoid": { "func": "wc_map", "arg": 0 } }),
        }
    }

    /// Slice widths must track the per-machine map-worker count, not 50/50.
    /// 8 maps on node 0 + 2 on node 1 → slices [0,0.8] and [0.8,1.0].
    #[test]
    fn input_slices_weighted_by_map_count() {
        let path = "corpus.txt";
        let mut nodes = vec![input_node("load_0", 0, path), input_node("load_1", 1, path)];
        let mut worker_ids = HashSet::new();
        for i in 0..8 { let id = format!("map_0_{i}"); worker_ids.insert(id.clone()); nodes.push(map_node(&id, 0)); }
        for i in 0..2 { let id = format!("map_1_{i}"); worker_ids.insert(id.clone()); nodes.push(map_node(&id, 1)); }
        let shared: HashSet<String> = std::iter::once(path.to_string()).collect();

        assign_input_slices(&mut nodes, 2, &worker_ids, &shared);

        let slice_of = |n: &SymbolicNode| n.kind["Input"]["slice"].clone();
        assert_eq!(slice_of(&nodes[0]), json!([0.0, 0.8]), "node 0 (8 maps) → 0.0..0.8");
        assert_eq!(slice_of(&nodes[1]), json!([0.8, 1.0]), "node 1 (2 maps) → 0.8..1.0");
    }

    /// fanout is clamped to the cluster core budget (Σ max(1, cores-reserve)).
    #[test]
    fn fanout_capped_to_core_budget() {
        let reserve = node_agent_common::PARTITIONER_FANOUT_CORES_RESERVED_PER_HOST;
        let mut hints = PlacementHints::default();
        hints.cores.insert(0, 16);
        hints.cores.insert(1, 16);
        // 2 hosts × (16 - reserve) usable cores.
        let budget = 2 * (16 - reserve);
        assert_eq!(cap_fanout(50, 2, &hints), budget, "over-large fanout clamped to budget");
        assert_eq!(cap_fanout(8, 2, &hints), 8, "fanout under budget is untouched");
        // No core data → never capped (standalone CLI).
        assert_eq!(cap_fanout(50, 2, &PlacementHints::default()), 50);
    }

    /// Equal map counts → even split (current placement:"all" reality).
    #[test]
    fn input_slices_balanced_when_maps_even() {
        let path = "corpus.txt";
        let mut nodes = vec![input_node("load_0", 0, path), input_node("load_1", 1, path)];
        let mut worker_ids = HashSet::new();
        for m in 0..2 { for i in 0..8 { let id = format!("map_{m}_{i}"); worker_ids.insert(id.clone()); nodes.push(map_node(&id, m)); } }
        let shared: HashSet<String> = std::iter::once(path.to_string()).collect();

        assign_input_slices(&mut nodes, 2, &worker_ids, &shared);

        assert_eq!(nodes[0].kind["Input"]["slice"], json!([0.0, 0.5]));
        assert_eq!(nodes[1].kind["Input"]["slice"], json!([0.5, 1.0]));
    }

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

    const SPLIT_PIPELINE_SYMBOLIC: &str = r#"{
      "shm_path_prefix": "/dev/shm/sp", "total_nodes": 2, "transfer": true,
      "nodes": [
        { "id": "load", "node_id": 0, "deps": [],
          "kind": { "Input": { "paths": ["a","b"], "slot": 10, "binary": true } } },
        { "id": "pipe", "deps": ["load"], "kind": { "StreamPipeline": {
            "rounds": 3, "cut_after": 0, "stages": [
              { "func": "s0", "arg0": 10, "arg1": 20 },
              { "func": "s1", "arg0": 20, "arg1": 30, "width": 2 },
              { "func": "s2", "arg0": 30, "arg1": 5 } ] } } },
        { "id": "save", "deps": ["pipe"],
          "kind": { "Output": { "slot": 5, "split_records": true, "paths": ["o0","o1"] } } }
      ] }"#;

    #[test]
    fn split_stream_pipeline_at_cut() {
        let dag = SymbolicDag::from_json(SPLIT_PIPELINE_SYMBOLIC).expect("parse");
        let out = partition(&dag, None).expect("partition");
        let nd = out["node_dags"].as_object().unwrap();
        let n0: Vec<Value> = serde_json::from_value(nd["0"].clone()).unwrap();
        let n1: Vec<Value> = serde_json::from_value(nd["1"].clone()).unwrap();
        let find = |ns: &[Value], id: &str| ns.iter().find(|n| n["id"].as_str() == Some(id)).cloned();

        // Upstream segment on node 0: stages[0..=0], embedded rdma_send of the boundary (20).
        let a = find(&n0, "pipe__s0").expect("pipe__s0 on node 0");
        let asp = &a["kind"]["StreamPipeline"];
        assert_eq!(asp["stages"].as_array().unwrap().len(), 1, "pipe__s0 keeps 1 stage");
        assert_eq!(asp["rdma_send"]["slot"], json!(20));
        assert_eq!(asp["rdma_send"]["peer"], json!(1));
        assert_eq!(asp["rdma_send"]["slot_kind"], json!("Stream"));
        assert!(asp.get("cut_after").is_none(), "cut_after must not leak to the executor");

        // Downstream segment on node 1: stages[1..], rdma_recv(boundary) + rdma_send(return,Io).
        let b = find(&n1, "pipe__s1").expect("pipe__s1 on node 1");
        let bsp = &b["kind"]["StreamPipeline"];
        assert_eq!(bsp["stages"].as_array().unwrap().len(), 2, "pipe__b keeps 2 stages");
        assert_eq!(bsp["stages"][0]["width"], json!(2), "per-stage width survives the split");
        assert_eq!(bsp["rdma_recv"]["slot"], json!(20));
        assert_eq!(bsp["rdma_send"]["slot"], json!(5));
        assert_eq!(bsp["rdma_send"]["slot_kind"], json!("Io"));
        assert_eq!(bsp["rdma_send"]["peer"], json!(0));

        // Terminal Output → StreamOutput sink on node 0, recv(return,Io) → paths.
        let sink = find(&n0, "save").expect("save sink on node 0");
        let so = &sink["kind"]["StreamOutput"];
        assert_eq!(so["slot"], json!(5));
        assert_eq!(so["rdma_recv"]["peer"], json!(1));
        assert_eq!(so["paths"].as_array().unwrap().len(), 2);
        assert!(sink["kind"].get("Output").is_none(), "Output must be replaced by StreamOutput");

        // The original opaque pipeline node is gone; no stray cross-edge RemoteSend/Recv.
        assert!(find(&n0, "pipe").is_none() && find(&n1, "pipe").is_none());
        assert!(!n0.iter().chain(&n1).any(|n| n["id"].as_str().map_or(false, |s| s.starts_with("rs_") || s.starts_with("rr_"))),
                "split halves use embedded RDMA, not injected RemoteSend/Recv nodes");
    }

    const SPLIT_3WAY_SYMBOLIC: &str = r#"{
      "shm_path_prefix": "/dev/shm/s3", "total_nodes": 3, "transfer": true,
      "nodes": [
        { "id": "load", "node_id": 0, "deps": [],
          "kind": { "Input": { "paths": ["a"], "slot": 10, "binary": true } } },
        { "id": "pipe", "deps": ["load"], "kind": { "StreamPipeline": {
            "rounds": 3, "segments": 3, "stages": [
              { "func": "s0", "arg0": 10, "arg1": 20, "out_weight": 5 },
              { "func": "s1", "arg0": 20, "arg1": 30, "split": "prefer" },
              { "func": "s2", "arg0": 30, "arg1": 40, "split": "avoid"  },
              { "func": "s3", "arg0": 40, "arg1": 50, "out_weight": 0.1 },
              { "func": "s4", "arg0": 50, "arg1": 5 } ] } } },
        { "id": "save", "deps": ["pipe"],
          "kind": { "Output": { "slot": 5, "split_records": true, "paths": ["o0","o1"] } } }
      ] }"#;

    #[test]
    fn split_three_way_auto_cut() {
        let dag = SymbolicDag::from_json(SPLIT_3WAY_SYMBOLIC).expect("parse");
        // Empty hints → machine_order is deterministic 0,1,2.
        let out = partition(&dag, Some(&PlacementHints::default())).expect("partition");
        let nd = out["node_dags"].as_object().unwrap();
        let all: Vec<Value> = (0..3).flat_map(|h|
            serde_json::from_value::<Vec<Value>>(nd[&h.to_string()].clone()).unwrap()).collect();
        let find = |id: &str| all.iter().find(|n| n["id"].as_str() == Some(id)).cloned();
        let on = |id: &str| -> i64 {
            for h in 0..3 {
                let ns: Vec<Value> = serde_json::from_value(nd[&h.to_string()].clone()).unwrap();
                if ns.iter().any(|n| n["id"].as_str() == Some(id)) { return h; }
            }
            -1
        };

        // 3 segments → cuts chosen at the two CHEAPEST boundaries: after s1 (prefer→0)
        // and after s3 (out_weight 0.1). s2 (avoid) is never a boundary.
        let s0 = find("pipe__s0").expect("s0"); let s0sp = &s0["kind"]["StreamPipeline"];
        let s1 = find("pipe__s1").expect("s1"); let s1sp = &s1["kind"]["StreamPipeline"];
        let s2 = find("pipe__s2").expect("s2"); let s2sp = &s2["kind"]["StreamPipeline"];

        // Stage partition: [s0,s1] | [s2,s3] | [s4]
        assert_eq!(s0sp["stages"].as_array().unwrap().len(), 2);
        assert_eq!(s1sp["stages"].as_array().unwrap().len(), 2);
        assert_eq!(s2sp["stages"].as_array().unwrap().len(), 1);

        // Placement on machines 0,1,2 (homogeneous → sequential).
        assert_eq!((on("pipe__s0"), on("pipe__s1"), on("pipe__s2")), (0, 1, 2));

        // Chain wiring: boundary slots are 30 (after s1) and 50 (after s3).
        assert_eq!(s0sp["rdma_send"]["slot"], json!(30)); assert_eq!(s0sp["rdma_send"]["peer"], json!(1));
        assert!(s0sp.get("rdma_recv").is_none());
        assert_eq!(s1sp["rdma_recv"]["slot"], json!(30)); assert_eq!(s1sp["rdma_recv"]["peer"], json!(0));
        assert_eq!(s1sp["rdma_send"]["slot"], json!(50)); assert_eq!(s1sp["rdma_send"]["peer"], json!(2));
        assert_eq!(s2sp["rdma_recv"]["slot"], json!(50)); assert_eq!(s2sp["rdma_recv"]["peer"], json!(1));
        assert_eq!(s2sp["rdma_send"]["slot"], json!(5));  // return, Io
        assert_eq!(s2sp["rdma_send"]["slot_kind"], json!("Io"));
        assert_eq!(s2sp["rdma_send"]["peer"], json!(0));

        // The `avoid` stage's output slot (40) is never a cut boundary.
        let wire = out.to_string();
        assert!(!wire.contains("\"slot\":40"), "avoid boundary (slot 40) must not be a cut");

        // Sink on node 0 pulls the return from the last segment's machine (2).
        let sink = find("save").expect("sink");
        assert_eq!(sink["kind"]["StreamOutput"]["rdma_recv"]["peer"], json!(2));
        assert_eq!(sink["kind"]["StreamOutput"]["slot"], json!(5));
    }
}
