//! Coordinator-side file staging: read shared input files and build the
//! StageFiles payload that is sent to workers before job assignment.

use anyhow::{Context, Result};
use crate::cluster_dag::SharedInput;
use crate::protocol::{StagedFile, StageFilesPayload};

/// Read the files listed in `shared_inputs` that are owned by `own_node_id`
/// and return a `StageFilesPayload` ready to send to remote workers.
///
/// Files whose `source_node` differs from `own_node_id` are skipped with a
/// warning (MVP: coordinator is always the file owner).
pub fn prepare_payload(
    job_id: &str,
    own_node_id: u32,
    shared_inputs: &[SharedInput],
) -> Result<StageFilesPayload> {
    let mut files = Vec::new();
    for input in shared_inputs {
        if input.source_node != own_node_id {
            eprintln!(
                "[staging] shared_input '{}' has source_node={} but own_node_id={}; skipping",
                input.path, input.source_node, own_node_id
            );
            continue;
        }
        let data = std::fs::read(&input.path)
            .with_context(|| format!("read shared input file: {}", input.path))?;
        println!("[staging] staging '{}' ({} bytes)", input.path, data.len());
        files.push(StagedFile {
            rel_path: input.path.clone(),
            data,
        });
    }
    Ok(StageFilesPayload {
        job_id: job_id.to_string(),
        files,
    })
}
