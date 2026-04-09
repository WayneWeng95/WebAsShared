use anyhow::{anyhow, Result};
use std::collections::{HashMap, VecDeque};
use crate::runtime::mem_operation::reclaimer::SlotKind;
use crate::runtime::input_output::logger::Level;
use super::types::*;

// ─── Logger helpers ───────────────────────────────────────────────────────────

/// Parse a user-supplied level string into a `Level`.
/// Returns `None` for `"off"` or an unrecognised value (logging disabled).
pub(super) fn parse_level(s: &str) -> Option<Level> {
    match s.to_ascii_lowercase().as_str() {
        "debug" => Some(Level::Debug),
        "info"  => Some(Level::Info),
        "warn"  => Some(Level::Warn),
        "error" => Some(Level::Error),
        _       => None,
    }
}

// ─── Slot bounds validation ───────────────────────────────────────────────────

/// Verify that all explicitly declared stream slot IDs are within
/// `[0, STREAM_SLOT_COUNT)` and all I/O slot IDs are within `[0, IO_SLOT_COUNT)`.
///
/// Stream slots and I/O slots are now completely separate — no slot in either
/// range is reserved for framework use, so the only constraint is that IDs
/// stay in bounds.
pub(super) fn validate_dag(dag: &Dag) -> Result<()> {
    use common::{IO_SLOT_COUNT, STREAM_SLOT_COUNT};
    let mut errors: Vec<String> = Vec::new();

    for node in &dag.nodes {
        // Collect stream slot IDs declared by routing/pipeline/watch/persist nodes.
        let mut stream_slots: Vec<usize> = Vec::new();
        // Collect I/O slot IDs declared by Input/Output nodes.
        let mut io_slots: Vec<(usize, &str)> = Vec::new(); // (slot, kind_label)

        match &node.kind {
            NodeKind::Bridge(p) => {
                stream_slots.push(p.from);
                stream_slots.push(p.to);
            }
            NodeKind::Aggregate(p) => {
                stream_slots.extend_from_slice(&p.upstream);
                stream_slots.push(p.downstream);
            }
            NodeKind::Shuffle(p) => {
                stream_slots.extend_from_slice(&p.upstream);
                stream_slots.extend_from_slice(&p.downstream);
            }
            NodeKind::StreamPipeline(p) => {
                for s in &p.stages {
                    stream_slots.push(s.arg0 as usize);
                    if let Some(a1) = s.arg1 { stream_slots.push(a1 as usize); }
                }
            }
            NodeKind::Watch(p) => {
                if let Some(s) = p.stream { stream_slots.push(s); }
            }
            NodeKind::Persist(p) => {
                stream_slots.extend_from_slice(&p.stream_slots);
            }
            NodeKind::Input(p) => {
                if let Some(s) = p.slot { io_slots.push((s as usize, "Input")); }
            }
            NodeKind::Output(p) => {
                if let Some(s) = p.slot { io_slots.push((s as usize, "Output")); }
            }
            NodeKind::RemoteSend(p) => {
                match p.slot_kind {
                    RemoteSlotKind::Stream => stream_slots.push(p.slot),
                    RemoteSlotKind::Io    => io_slots.push((p.slot, "RemoteSend")),
                }
                // The slot producer must be a DAG dep so it finishes before
                // the RDMA DMA reads from those pages.  A missing dep means
                // the producer could still be writing while we're reading.
                if node.deps.is_empty() {
                    errors.push(format!(
                        "node '{}' (RemoteSend): must list the slot producer as a \
                         dependency to guarantee the page chain is sealed before RDMA.",
                        node.id
                    ));
                }
                if dag.rdma.as_ref().map_or(true, |r| !r.transfer) {
                    errors.push(format!(
                        "node '{}' (RemoteSend): requires dag.rdma with transfer=true.",
                        node.id
                    ));
                }
            }
            NodeKind::RemoteRecv(p) => {
                match p.slot_kind {
                    RemoteSlotKind::Stream => stream_slots.push(p.slot),
                    RemoteSlotKind::Io    => io_slots.push((p.slot, "RemoteRecv")),
                }
                if dag.rdma.as_ref().map_or(true, |r| !r.transfer) {
                    errors.push(format!(
                        "node '{}' (RemoteRecv): requires dag.rdma with transfer=true.",
                        node.id
                    ));
                }
            }
            _ => {}
        }

        for s in stream_slots {
            if s >= STREAM_SLOT_COUNT {
                errors.push(format!(
                    "node '{}': stream slot {} ≥ STREAM_SLOT_COUNT ({})",
                    node.id, s, STREAM_SLOT_COUNT
                ));
            }
        }
        for (s, label) in io_slots {
            if s >= IO_SLOT_COUNT {
                errors.push(format!(
                    "node '{}': {} I/O slot {} ≥ IO_SLOT_COUNT ({})",
                    node.id, label, s, IO_SLOT_COUNT
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("DAG validation failed:\n  {}", errors.join("\n  ")))
    }
}

// ─── Slot lifetime tracking ───────────────────────────────────────────────────

/// Slots whose **metadata only** should be zeroed after `node` finishes.
///
/// Routing operations (Bridge, Aggregate, Shuffle, Broadcast) splice the
/// upstream page chains into downstream chains via `next_offset` links.
/// After routing the upstream slot's `writer_heads`/`writer_tails` still
/// point into pages that are now owned by the downstream chain.  We must
/// zero only the metadata — `clear_stream_slot` — not free the pages;
/// freeing would corrupt the downstream chain and cause the walker in
/// `free_page_chain` to chase into the free-list or into reallocated pages.
pub(super) fn node_routed_upstream_slots(kind: &NodeKind) -> Vec<usize> {
    match kind {
        NodeKind::Bridge(p)    => vec![p.from],
        NodeKind::Aggregate(p) => p.upstream.clone(),
        NodeKind::Shuffle(p)   => p.upstream.clone(),
        _ => vec![],
    }
}

/// Slots whose **pages should be freed** after `node` finishes.
///
/// Only slots with *exclusive* page ownership are listed here — i.e. no
/// routing operation has spliced those pages into another slot's chain.
///
/// - I/O Output slots: written by the SlotLoader, read by the guest, drained
///   by the SlotFlusher.  No routing ever touches the I/O area.
/// - StreamPipeline `source_slot`: written by an upstream node and read
///   only by this pipeline; ownership is unambiguous.
///
/// Stream slots involved in routing use `node_routed_upstream_slots` instead.
/// Watch/Persist read stream slots but do not own them, so they are skipped
/// (they are freed by whichever node actually consumes the data).
pub(super) fn node_owned_slots(kind: &NodeKind) -> (Vec<usize>, Vec<usize>) {
    use common::OUTPUT_IO_SLOT;
    // (stream_slots_to_free, io_slots_to_free)
    match kind {
        NodeKind::Output(p) =>
            (vec![], vec![p.slot.unwrap_or(OUTPUT_IO_SLOT) as usize]),
        NodeKind::StreamPipeline(p) =>
            // stages[0].arg0 is the pipeline's source slot, owned by the upstream node.
            (p.stages.first().map(|s| vec![s.arg0 as usize]).unwrap_or_default(), vec![]),
        _ => (vec![], vec![]),
    }
}

/// Scan the full DAG and build reader-count maps for slots that will be freed.
///
/// Only counts slots tracked by `node_owned_slots` — slots with exclusive
/// page ownership.  Routing upstreams are counted separately via
/// `node_routed_upstream_slots` (they only need a metadata clear, not a free).
pub(super) fn build_slot_refcounts(dag: &Dag) -> HashMap<(SlotKind, usize), usize> {
    let mut counts: HashMap<(SlotKind, usize), usize> = HashMap::new();
    for node in &dag.nodes {
        let (streams, ios) = node_owned_slots(&node.kind);
        for s in streams {
            *counts.entry((SlotKind::Stream, s)).or_insert(0) += 1;
        }
        for s in ios {
            *counts.entry((SlotKind::Io, s)).or_insert(0) += 1;
        }
    }
    counts
}

// ─── Topological sort (Kahn's algorithm) ─────────────────────────────────────

pub(super) fn topo_sort(nodes: &[DagNode]) -> Result<Vec<usize>> {
    let id_to_idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();

    let mut in_degree = vec![0usize; nodes.len()];
    // adj[i] = list of nodes that depend on node i (i must finish before them)
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];

    for (i, node) in nodes.iter().enumerate() {
        for dep_id in &node.deps {
            let dep_idx = *id_to_idx
                .get(dep_id.as_str())
                .ok_or_else(|| anyhow!("Unknown dep '{}' in node '{}'", dep_id, node.id))?;
            adj[dep_idx].push(i);
            in_degree[i] += 1;
        }
    }

    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, &d)| d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut order = Vec::with_capacity(nodes.len());
    while let Some(n) = queue.pop_front() {
        order.push(n);
        for &m in &adj[n] {
            in_degree[m] -= 1;
            if in_degree[m] == 0 {
                queue.push_back(m);
            }
        }
    }

    if order.len() != nodes.len() {
        return Err(anyhow!("DAG contains a cycle — cannot execute"));
    }
    Ok(order)
}

// ─── Wave builder and WASM node classifier ───────────────────────────────────

/// Compute execution waves: groups of nodes that can run concurrently.
/// All nodes in a wave have all their dependencies in earlier waves.
pub(super) fn build_waves(nodes: &[DagNode], order: &[usize]) -> Vec<Vec<usize>> {
    let id_to_idx: HashMap<&str, usize> = nodes.iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let mut level = vec![0usize; nodes.len()];
    for &idx in order {
        for dep_id in &nodes[idx].deps {
            if let Some(&dep_idx) = id_to_idx.get(dep_id.as_str()) {
                level[idx] = level[idx].max(level[dep_idx] + 1);
            }
        }
    }
    let max_level = level.iter().copied().max().unwrap_or(0);
    let mut waves: Vec<Vec<usize>> = vec![Vec::new(); max_level + 1];
    for &idx in order {
        waves[level[idx]].push(idx);
    }
    waves
}

/// Validate that all nodes sharing the same `barrier_group` are placed in the
/// same wave.  Returns an error listing each group whose members span multiple
/// waves.
pub(super) fn validate_barrier_groups(nodes: &[DagNode], waves: &[Vec<usize>]) -> Result<()> {
    // Build a map: node index → wave index.
    let mut node_wave: Vec<usize> = vec![0; nodes.len()];
    for (wi, wave) in waves.iter().enumerate() {
        for &idx in wave {
            node_wave[idx] = wi;
        }
    }

    // Group nodes by barrier_group name and check wave consistency.
    let mut groups: HashMap<&str, (usize, Vec<&str>)> = HashMap::new(); // name → (wave, [node_ids])
    let mut errors: Vec<String> = Vec::new();

    for (idx, node) in nodes.iter().enumerate() {
        if let Some(ref group) = node.barrier_group {
            let w = node_wave[idx];
            match groups.get_mut(group.as_str()) {
                Some((expected_wave, members)) => {
                    if *expected_wave != w {
                        errors.push(format!(
                            "barrier_group '{}': node '{}' is in wave {} but group expects wave {} \
                             (members so far: {})",
                            group, node.id, w, expected_wave, members.join(", ")
                        ));
                    }
                    members.push(&node.id);
                }
                None => {
                    groups.insert(group.as_str(), (w, vec![&node.id]));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("Barrier group validation failed:\n  {}", errors.join("\n  ")))
    }
}

/// Compute per-wave barrier assignments: for each wave that contains nodes with
/// `barrier_group`, assign sequential barrier slot IDs (0, 1, ...) and return
/// the set of barrier IDs that need resetting before each wave.
///
/// Returns `(wave_index → Vec<barrier_id_to_reset>, group_name → (barrier_id, party_count))`.
pub(super) fn build_barrier_assignments(
    nodes: &[DagNode],
    waves: &[Vec<usize>],
) -> (Vec<Vec<usize>>, HashMap<String, (usize, usize)>) {
    let mut global_id: usize = 0;
    let mut group_map: HashMap<String, (usize, usize)> = HashMap::new(); // name → (barrier_id, party_count)
    let mut wave_barriers: Vec<Vec<usize>> = vec![Vec::new(); waves.len()];

    for (wi, wave) in waves.iter().enumerate() {
        // Collect distinct groups in this wave.
        let mut wave_groups: HashMap<&str, usize> = HashMap::new(); // name → count

        for &idx in wave {
            if let Some(ref group) = nodes[idx].barrier_group {
                *wave_groups.entry(group.as_str()).or_insert(0) += 1;
            }
        }

        for (name, count) in wave_groups {
            if !group_map.contains_key(name) {
                assert!(global_id < common::BARRIER_COUNT,
                    "Too many barrier groups (max {})", common::BARRIER_COUNT);
                group_map.insert(name.to_string(), (global_id, count));
                wave_barriers[wi].push(global_id);
                global_id += 1;
            }
        }
    }

    (wave_barriers, group_map)
}

/// Returns true for node kinds executed as isolated subprocesses
/// (WasmVoid/WasmU32/WasmFatPtr and PyFunc).
pub(super) fn is_oneshot_node(kind: &NodeKind) -> bool {
    matches!(kind,
        NodeKind::WasmVoid(_)
        | NodeKind::WasmU32(_)
        | NodeKind::WasmFatPtr(_)
        | NodeKind::PyFunc(_)
    )
}
