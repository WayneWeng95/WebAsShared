fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);

    let mut dag_path = "DAGs/symbolic_dag/word_count.json".to_string();
    let mut hints_path: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hints" => {
                hints_path = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--hints requires a path argument"))?,
                );
            }
            _ => dag_path = arg,
        }
    }

    let raw = std::fs::read_to_string(&dag_path)
        .map_err(|e| anyhow::anyhow!("read {}: {}", dag_path, e))?;
    let dag = partitioner::SymbolicDag::from_json(&raw)?;

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
