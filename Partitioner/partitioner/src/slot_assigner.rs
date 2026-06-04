use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::placer::topo_order;
use crate::symbolic_dag::SymbolicNode;

const INPUT_IO_SLOT: u32 = 0;
const OUTPUT_IO_SLOT: u32 = 1;

/// Assign concrete slot numbers to every node in the DAG, patching both
/// `node.output_slot` and the inline kind JSON.
///
/// This is called after `assign_nodes()` (placement) and before cross-node
/// edge detection, so all `node_id` fields are already filled in.
///
/// Func kind declarations recognised:
///
/// | Field            | Meaning                                                           |
/// |------------------|-------------------------------------------------------------------|
/// | `out_base: N`    | Fan-out function (e.g. wc_distribute).  Assigns slots            |
/// |                  | [N, N+1, …] to each direct consumer in array order.  Sets       |
/// |                  | `arg = base`; the splitter later packs the consumer count into  |
/// |                  | the high bits when emitting the WasmVoid kind.                  |
/// | `output_offset: K` | Output = arg + K (e.g. wc_map with K=100).  `arg` is the        |
/// |                  | slot assigned by the upstream fan-out (or dep's output slot).    |
/// | neither          | Terminal function.  `arg` = dep's output slot,                   |
/// |                  | output = OUTPUT_IO_SLOT (1).                                      |
///
/// Aggregate kind:
///   If neither `upstream_nodes` nor `upstream` is present, derives
///   `upstream_nodes` from `deps`.  Always allocates a fresh `downstream` slot.
///
/// Input kind:  inserts `slot: INPUT_IO_SLOT (0)` when absent.
/// Output kind: inserts `slot: dep_output_slot` so the splitter can rewrite it
///              for cross-node cases.
pub(crate) fn assign_slots(nodes: &mut Vec<SymbolicNode>) -> Result<()> {
    let order = topo_order(nodes);

    // id → output slot produced by each node
    let mut id_to_output: HashMap<String, u32> = HashMap::new();

    // (fanout_src_id, consumer_id) → the specific input slot the fan-out assigned
    let mut consumer_input: HashMap<(String, String), u32> = HashMap::new();

    // fan-out node id → effective (possibly auto-adjusted) output base slot.
    // The adjusted base is injected as `arg` so the WASM writes to the right slots.
    let mut fanout_adjusted_base: HashMap<String, u32> = HashMap::new();

    // next free slot; starts at 2 so it sits above the two IO slots.
    // Fan-out processing (phase 1 below) will push it past any hardcoded ranges
    // (e.g. wc_distribute base=10 pushes it to ≥20, wc_map offset=100 to ≥120).
    let mut next_slot: u32 = 2;

    // ── Phase 1: fan-out pre-pass with per-machine overlap auto-adjustment ────
    //
    // Two fan-out nodes that land on the same machine cannot share slot numbers —
    // both would write to the same SHM region and map workers would read stale or
    // interleaved data and stall.  We detect this from the JSON topology and shift
    // the second node's base to start immediately after the first's range ends,
    // then inject the corrected base as `arg` so the WASM writes to the right slots.
    //
    // Collect fan-out entries: (machine, original_base, node_idx), topo-ordered.
    let fanout_entries: Vec<(u32, u32, usize)> = order
        .iter()
        .filter_map(|&idx| {
            let func = nodes[idx].kind.get("Func")?;
            let base = func.get("out_base")?.as_u64()? as u32;
            let machine = nodes[idx].node_id?;
            Some((machine, base, idx))
        })
        .collect();

    // Group by machine (stable order within each machine = topo order).
    let mut machines: Vec<u32> = fanout_entries.iter().map(|&(m, _, _)| m).collect();
    machines.sort_unstable();
    machines.dedup();

    for machine in machines {
        let mut next_free: u32 = 0;
        for &(m, orig_base, idx) in &fanout_entries {
            if m != machine {
                continue;
            }
            let eff_base = orig_base.max(next_free);
            if eff_base != orig_base {
                eprintln!(
                    "[partitioner] auto-adjusted out_base for '{}' on machine {}: \
                     {} → {} to avoid slot overlap",
                    nodes[idx].id, machine, orig_base, eff_base
                );
            }
            let src_id = nodes[idx].id.clone();
            let consumers: Vec<usize> = (0..nodes.len())
                .filter(|&i| nodes[i].deps.iter().any(|d| d == &src_id))
                .collect();
            let n = consumers.len() as u32;
            for (i, &cidx) in consumers.iter().enumerate() {
                let slot = eff_base + i as u32;
                consumer_input.insert((src_id.clone(), nodes[cidx].id.clone()), slot);
                next_slot = next_slot.max(slot + 1);
            }
            fanout_adjusted_base.insert(src_id, eff_base);
            next_free = eff_base + n;
        }
    }

    // ── Phase 2: output slot assignment ──────────────────────────────────────
    for &idx in &order {
        let node_id = nodes[idx].id.clone();
        let node_deps = nodes[idx].deps.clone();
        let kind = nodes[idx].kind.clone();

        if kind.get("Input").is_some() {
            let input_obj = kind["Input"].as_object().unwrap();
            let slot = input_obj
                .get("slot")
                .and_then(|v| v.as_u64())
                .unwrap_or(INPUT_IO_SLOT as u64) as u32;
            let mut new_in = input_obj.clone();
            new_in.insert("slot".into(), json!(slot));
            nodes[idx].kind = json!({ "Input": Value::Object(new_in) });
            id_to_output.insert(node_id, slot);

        } else if let Some(func) = kind.get("Func").and_then(|v| v.as_object()) {
            // Backward compat: if 'arg' is already specified the caller manually set all
            // slots.  Record the output slot (from 'arg2' if present, else OUTPUT_IO_SLOT)
            // so downstream nodes can find it, but do not overwrite the kind.
            if func.contains_key("arg") {
                let out_slot = func
                    .get("arg2")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .unwrap_or(OUTPUT_IO_SLOT);
                nodes[idx].output_slot = nodes[idx].output_slot.or(Some(out_slot));
                id_to_output.insert(node_id, out_slot);
                next_slot = next_slot.max(out_slot + 1);
                continue;
            }

            let out_base = func.get("out_base").and_then(|v| v.as_u64());
            let out_offset = func.get("output_offset").and_then(|v| v.as_u64());

            if out_base.is_some() {
                // Fan-out: arg = effective base slot (possibly auto-adjusted in
                // Phase 1).  The splitter packs the consumer count into the high
                // bits of `arg` when it emits the WasmVoid kind — doing it here
                // would pollute the partitioner's slot-number bookkeeping
                // (collect_slots would read the packed value as a used slot).
                let eff_base = fanout_adjusted_base
                    .get(&node_id)
                    .copied()
                    .unwrap_or(out_base.unwrap() as u32);
                let mut new_func = func.clone();
                new_func.insert("arg".into(), json!(eff_base));
                nodes[idx].kind = json!({ "Func": Value::Object(new_func) });
                id_to_output.insert(node_id, eff_base);

            } else if let Some(offset) = out_offset {
                // Offset-output: arg = slot assigned by upstream fan-out (or dep output)
                let in_slot = input_slot_for(&node_id, &node_deps, &consumer_input, &id_to_output)?;
                let out_slot = in_slot + offset as u32;
                let mut new_func = func.clone();
                new_func.insert("arg".into(), json!(in_slot));
                nodes[idx].kind = json!({ "Func": Value::Object(new_func) });
                nodes[idx].output_slot = Some(out_slot);
                id_to_output.insert(node_id, out_slot);
                next_slot = next_slot.max(out_slot + 1);

            } else {
                // Terminal: arg = dep's output slot, this node writes to OUTPUT_IO_SLOT
                let dep_slot = dep_output_slot(&node_id, &node_deps, &id_to_output)?;
                let mut new_func = func.clone();
                new_func.insert("arg".into(), json!(dep_slot));
                nodes[idx].kind = json!({ "Func": Value::Object(new_func) });
                nodes[idx].output_slot = Some(OUTPUT_IO_SLOT);
                id_to_output.insert(node_id, OUTPUT_IO_SLOT);
            }

        } else if let Some(agg) = kind.get("Aggregate").and_then(|v| v.as_object()) {
            // Pass through if already fully specified
            if agg.contains_key("upstream") && agg.contains_key("downstream") {
                let ds = agg["downstream"].as_u64().unwrap() as u32;
                nodes[idx].output_slot = nodes[idx].output_slot.or(Some(ds));
                id_to_output.insert(node_id, ds);
                next_slot = next_slot.max(ds + 1);
                continue;
            }

            let mut new_agg = agg.clone();

            // Populate upstream_nodes from deps when absent
            if !new_agg.contains_key("upstream_nodes") && !new_agg.contains_key("upstream") {
                new_agg.insert("upstream_nodes".into(), json!(node_deps));
            }

            // Allocate downstream slot if absent
            let ds = match agg.get("downstream").and_then(|v| v.as_u64()) {
                Some(v) => v as u32,
                None => {
                    let s = next_slot;
                    next_slot += 1;
                    s
                }
            };
            new_agg.insert("downstream".into(), json!(ds));
            nodes[idx].kind = json!({ "Aggregate": Value::Object(new_agg) });
            nodes[idx].output_slot = Some(ds);
            id_to_output.insert(node_id, ds);
            next_slot = next_slot.max(ds + 1);

        } else if let Some(out_obj) = kind.get("Output").and_then(|v| v.as_object()) {
            // Record the dep's output slot so the splitter can rewrite for cross-node.
            let dep_slot = dep_output_slot(&node_id, &node_deps, &id_to_output)?;
            let mut new_out = out_obj.clone();
            new_out.insert("slot".into(), json!(dep_slot));
            nodes[idx].kind = json!({ "Output": Value::Object(new_out) });
            // Output nodes have no output_slot annotation.
        }
        // All other kinds (RemoteRecv, RemoteSend, Bridge, …) pass through unchanged.
    }

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Return the input slot for a node: prefers a fan-out assigned slot, falls
/// back to the dep's output slot.
fn input_slot_for(
    node_id: &str,
    deps: &[String],
    consumer_input: &HashMap<(String, String), u32>,
    id_to_output: &HashMap<String, u32>,
) -> Result<u32> {
    for dep_id in deps {
        if let Some(&slot) = consumer_input.get(&(dep_id.clone(), node_id.to_string())) {
            return Ok(slot);
        }
    }
    dep_output_slot(node_id, deps, id_to_output)
}

/// Return the output slot of the (last / only) dep.
fn dep_output_slot(
    node_id: &str,
    deps: &[String],
    id_to_output: &HashMap<String, u32>,
) -> Result<u32> {
    let dep_id = deps
        .last()
        .ok_or_else(|| anyhow!("node '{}' has no deps but needs an input slot", node_id))?;
    id_to_output.get(dep_id.as_str()).copied().ok_or_else(|| {
        anyhow!(
            "node '{}': dep '{}' has no assigned output slot (topo-order bug?)",
            node_id,
            dep_id
        )
    })
}
