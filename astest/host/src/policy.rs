use std::collections::HashMap;

/// A snapshot of a single conflicting write node, deserialized by the host-side organizer.
#[derive(Debug, Clone)]
pub struct HostNode {
    pub offset: u32,
    pub writer_id: u32,
    pub data_len: u32,
    pub registry_index: u32, 
    pub payload: Vec<u8>, 
}
/// The outcome of applying a `ConsumptionPolicy` to a bucket's conflict list.
#[derive(Debug)]
pub enum ConsumptionResult {
    /// The winning writer ID and its payload decoded as a UTF-8 string.
    Winner(u32, String),
    /// No nodes were present; nothing to commit.
    None,
}

/// Defines how the Manager resolves concurrent writes to the same key.
pub trait ConsumptionPolicy {
    /// Evaluates the set of conflicting nodes and returns the single winner.
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult;
}

/// Conflict policy: selects the node written by the writer with the highest ID.
pub struct MaxIdWinsPolicy;
impl ConsumptionPolicy for MaxIdWinsPolicy {
    /// Returns the node whose `writer_id` is the maximum among all candidates.
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult {
        if let Some(node) = nodes.iter().max_by_key(|n| n.writer_id) {
            let content = String::from_utf8_lossy(&node.payload).to_string();
            ConsumptionResult::Winner(node.writer_id, content)
        } else {
            ConsumptionResult::None
        }
    }
}

/// Conflict policy: selects the node written by the writer with the lowest ID.
pub struct MinIdWinsPolicy;
impl ConsumptionPolicy for MinIdWinsPolicy {
    /// Returns the node whose `writer_id` is the minimum among all candidates.
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult {
        if let Some(node) = nodes.iter().min_by_key(|n| n.writer_id) {
            let content = String::from_utf8_lossy(&node.payload).to_string();
            ConsumptionResult::Winner(node.writer_id, content)
        } else {
            ConsumptionResult::None
        }
    }
}

/// Conflict policy: selects the payload written by the largest number of writers.
pub struct MajorityWinsPolicy;
impl ConsumptionPolicy for MajorityWinsPolicy {
    /// Counts payload occurrences and returns the first node with the most frequent payload.
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult {
        let mut counts: HashMap<&Vec<u8>, usize> = HashMap::new();
        for node in nodes {
            *counts.entry(&node.payload).or_insert(0) += 1;
        }

        if let Some((payload, _count)) = counts.iter().max_by_key(|&(_, count)| count) {
            let writer_id = nodes.iter().find(|n| &n.payload == *payload).unwrap().writer_id;
            let content = String::from_utf8_lossy(payload).to_string();
            ConsumptionResult::Winner(writer_id, content)
        } else {
            ConsumptionResult::None
        }
    }
}

/// Conflict policy: selects the most recently written node (Last-Write-Wins / LWW).
/// Because the guest uses lock-free CAS head insertion, `nodes[0]` is always
/// the last successful write.
pub struct LastWriteWinsPolicy;

impl ConsumptionPolicy for LastWriteWinsPolicy {
    /// Returns `nodes[0]`, which is the last-inserted (most recent) write.
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult {
        if let Some(latest_node) = nodes.first() {
            let content = String::from_utf8_lossy(&latest_node.payload).to_string();

            println!("[Manager] LWW Picked Winner ID: {}, Payload: '{}'", latest_node.writer_id, content);

            ConsumptionResult::Winner(latest_node.writer_id, content)
        } else {
            ConsumptionResult::None
        }
    }
}

/// Conflict policy: selects the node with the largest payload; ties broken by highest writer ID.
pub struct LargestPayloadWinsPolicy;

impl ConsumptionPolicy for LargestPayloadWinsPolicy {
    /// Returns the node with the maximum `data_len`, using `writer_id` as a tiebreaker.
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult {
        if let Some(node) = nodes.iter().max_by_key(|n| (n.data_len, n.writer_id)) {
            let content = String::from_utf8_lossy(&node.payload).to_string();
            ConsumptionResult::Winner(node.writer_id, content)
        } else {
            ConsumptionResult::None
        }
    }
}
