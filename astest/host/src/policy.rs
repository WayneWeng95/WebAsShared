use std::collections::HashMap;

// Node summary
#[derive(Debug, Clone)]
pub struct HostNode {
    pub offset: u32,
    pub writer_id: u32,
    pub data_len: u32,
    pub registry_index: u32, 
    pub payload: Vec<u8>, 
}
// Consumption result enum
#[derive(Debug)]
pub enum ConsumptionResult {
    Winner(u32, String), // ID, Content
    None,
}

pub trait ConsumptionPolicy {
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult;
}

// ==========================================
// Policy A: Max ID wins
// ==========================================
pub struct MaxIdWinsPolicy;
impl ConsumptionPolicy for MaxIdWinsPolicy {
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult {
        if let Some(node) = nodes.iter().max_by_key(|n| n.writer_id) {
            let content = String::from_utf8_lossy(&node.payload).to_string();
            ConsumptionResult::Winner(node.writer_id, content)
        } else {
            ConsumptionResult::None
        }
    }
}

// ==========================================
// Policy B: Min ID wins
// ==========================================
pub struct MinIdWinsPolicy;
impl ConsumptionPolicy for MinIdWinsPolicy {
    fn process(&self, nodes: &Vec<HostNode>) -> ConsumptionResult {
        if let Some(node) = nodes.iter().min_by_key(|n| n.writer_id) {
            let content = String::from_utf8_lossy(&node.payload).to_string();
            ConsumptionResult::Winner(node.writer_id, content)
        } else {
            ConsumptionResult::None
        }
    }
}

// ==========================================
// Policy C: Majority wins (most frequent content)
// ==========================================
pub struct MajorityWinsPolicy;
impl ConsumptionPolicy for MajorityWinsPolicy {
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

// ==========================================
// Policy D: Last Write Wins (LWW) / Keep Latest
// ==========================================
// Because the Guest uses Lock-Free CAS Head Insertion, 
// the physical head of the linked list (nodes[0]) is ALWAYS 
// the absolute latest data that was successfully written.
pub struct LastWriteWinsPolicy;

impl ConsumptionPolicy for LastWriteWinsPolicy {
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