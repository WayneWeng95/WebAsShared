# RDMA-InputScale

Sweeps the **input file size** for the 2-node RDMA distributed word count
(`DAGs/symbolic_dag/word_count.json`) and records per-node + total wall time.

The SymbolicDag is auto-partitioned into:
- **node 0** — load → distribute → map×10 → local aggregate (slot 200) → **RemoteSend** 200 → exits.
- **node 1** — its own load → … → local aggregate (200), **RemoteRecv** node 0's 200 (→ 201),
  **aggregate_global** (200+201 → 300), **wc_reduce**, **save**.

So **node 1 is the receiver + global reducer and sits on the critical path**: it
can't finish the global merge until node 0's partial arrives, then adds the
reduce + output. That's why node 1 ≈ total wall time and ~1.8–2.1× node 0.

> Note: both nodes load and count the *full* corpus (the demo is not
> input-partitioned), so the result is 2× the true count and each node processes
> the whole file — keep sizes within the ~1.5 GiB guest window (≤ ~512 MB).
> The input file is staged to node 1 over RDMA (TCP fallback) before the job.

## Prerequisites
The coordinator + worker daemons must already be running:
```
# node 0 (10.10.1.2):  ./node-agent start --config NodeAgent/agent_coordinator.toml
# node 1 (10.10.1.1):  ./node-agent start --config NodeAgent/agent_worker.toml
```
`TestData/corpus.txt` must exist on node 0 (used as the seed to regenerate
`corpus_large.txt` at each size; only node 0 needs it — staging delivers it).

## Running
```
./run.sh                  # default sweep: 16 32 64 128 256 MB
./run.sh "64 256 512"     # custom sizes (MB)
./run.sh "128" 5          # sizes + repeats per size (median reported)
./plot.py                 # results.csv -> figs/rdma_input_scale.png
```

## Sample result (16/64/128/256 MB)

| input | node 0 | node 1 | total | n1/n0 |
|---|---|---|---|---|
| 16 MB | 1092 ms | 2274 ms | 2327 ms | 2.08× |
| 64 MB | 2370 ms | 4609 ms | 4661 ms | 1.94× |
| 128 MB | 3271 ms | 6255 ms | 6308 ms | 1.91× |
| 256 MB | 5184 ms | 9438 ms | 9490 ms | 1.82× |

Both nodes scale ~linearly with input; node 1 stays ~2× node 0, drifting toward
parity as the fixed reduce/transfer tail amortizes over larger inputs.

Outputs: `results.csv`, `figs/rdma_input_scale.png`.
