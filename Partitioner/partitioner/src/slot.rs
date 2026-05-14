use serde_json::Value;
use std::collections::BTreeSet;

/// Auto-detect the output slot of a node from its kind JSON.
///
/// Rules:
/// - `Aggregate { downstream: N }` → N
/// - `Bridge    { to: N }`         → N
/// - `Input     { slot: N }`       → N  (defaults to 0 = INPUT_IO_SLOT if absent)
///
/// Returns `None` for kinds where no slot is derivable (e.g., Func, WasmVoid).
/// In that case the caller must supply an explicit `output_slot` annotation.
pub fn output_slot(kind: &Value) -> Option<u32> {
    if let Some(agg) = kind.get("Aggregate") {
        return agg.get("downstream")?.as_u64().map(|n| n as u32);
    }
    if let Some(bridge) = kind.get("Bridge") {
        return bridge.get("to")?.as_u64().map(|n| n as u32);
    }
    if let Some(input) = kind.get("Input") {
        return Some(
            input.get("slot")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
        );
    }
    None
}

/// Recursively collect all non-negative integer values from a JSON value.
///
/// Used to find the current maximum slot number in use so the Partitioner
/// can allocate fresh, non-conflicting slot IDs for generated RemoteRecv nodes.
pub fn collect_slots(v: &Value, out: &mut BTreeSet<u32>) {
    match v {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                if u <= u32::MAX as u64 {
                    out.insert(u as u32);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_slots(item, out);
            }
        }
        Value::Object(map) => {
            for val in map.values() {
                collect_slots(val, out);
            }
        }
        _ => {}
    }
}
