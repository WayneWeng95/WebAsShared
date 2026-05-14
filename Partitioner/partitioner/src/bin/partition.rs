fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "DAGs/symbolic_dag/word_count.json".to_string());
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("read {}: {}", path, e))?;
    let dag = partitioner::SymbolicDag::from_json(&raw)?;
    let out = partitioner::partition(&dag)?;
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
