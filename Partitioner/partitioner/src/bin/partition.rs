fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);

    let mut dag_path = "DAGs/symbolic_dag/word_count.json".to_string();
    let mut hints_path: Option<String> = None;
    let mut nodes_override: Option<usize> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hints" => {
                hints_path = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--hints requires a path argument"))?,
                );
            }
            "--nodes" => {
                nodes_override = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--nodes requires a count argument"))?
                        .parse()
                        .map_err(|e| anyhow::anyhow!("--nodes must be an integer: {}", e))?,
                );
            }
            _ => dag_path = arg,
        }
    }

    let raw = std::fs::read_to_string(&dag_path)
        .map_err(|e| anyhow::anyhow!("read {}: {}", dag_path, e))?;
    let mut dag = partitioner::SymbolicDag::from_json(&raw)?;
    // total_nodes is optional in the DAG — the coordinator fills it from the live
    // cluster at submit time.  For standalone preview, take --nodes, else the
    // DAG's value, else default to 2 so multi-node placement is visible.
    if let Some(n) = nodes_override {
        dag.total_nodes = Some(n);
    } else if dag.total_nodes.is_none() {
        eprintln!("[partition] no total_nodes in DAG and no --nodes given; defaulting to 2 (use --nodes N)");
        dag.total_nodes = Some(2);
    }

    let hints: Option<partitioner::PlacementHints> = match hints_path {
        Some(ref path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read hints {}: {}", path, e))?;
            Some(serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("parse hints JSON: {}", e))?)
        }
        None => None,
    };

    let out = partitioner::partition(&dag, hints.as_ref())?;
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
