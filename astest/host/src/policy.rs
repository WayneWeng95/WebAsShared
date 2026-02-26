use std::collections::HashMap;

// Node summary
#[derive(Debug, Clone)]
pub struct HostNode {
    pub offset: u32,
    pub writer_id: u32,
    pub data_len: u32,
    pub payload: Vec<u8>, // Payload is read so we can do "most frequent content" analysis
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
        
        // Count frequency
        for node in nodes {
            *counts.entry(&node.payload).or_insert(0) += 1;
        }

        // Find the maximum
        if let Some((payload, _count)) = counts.iter().max_by_key(|&(_, count)| count) {
            // Pick any writer ID that has this payload
            let writer_id = nodes.iter().find(|n| &n.payload == *payload).unwrap().writer_id;
            let content = String::from_utf8_lossy(payload).to_string();
            ConsumptionResult::Winner(writer_id, content)
        } else {
            ConsumptionResult::None
        }
    }
}