# Partitioner

**Status: placeholder — design notes only, no implementation yet.**

Reserved for a future subsystem that takes a **symbolic, location-agnostic DAG**
and auto-partitions it into per-node DAGs suitable for the existing NodeAgent
submission path (`ClusterDag.node_dags`).

## Why this exists

Today, distributed workflows are authored as `ClusterDag` JSON with the
per-node split already done by hand. For example, `DAGs/cluster_dag/word_count.json`
contains two fully-enumerated DAGs under `node_dags["0"]` and `node_dags["1"]`,
complete with explicit `RemoteSend` / `RemoteRecv` node pairs, matching slot
numbers, and matching peer IDs. The author is responsible for:

- Deciding which computations live on which node
- Inserting `RemoteSend` / `RemoteRecv` at every cross-node edge
- Assigning compatible slot IDs on each side
- Keeping the two sides' DAGs structurally consistent

`NodeAgent/agent/src/cluster_dag.rs::split` only does trivial work on top of
this: it injects the RDMA mesh config (`node_id`, `total`, `ips`) into each
per-node DAG and ships them. No graph-level reasoning.

This works well for the current research workload set (~8 hand-written pipelines)
but doesn't scale to larger or dynamically-generated DAGs, and it makes it hard
to experiment with placement strategies without rewriting the JSON.

## What a Partitioner would do

Accept a single symbolic DAG describing the **logical** computation — nodes
and edges only, no node IDs or `Remote*` nodes — together with a placement
annotation (either explicit `node_id` per node, or a cost-based placement
policy). Produce a `ClusterDag` with the per-node DAGs filled in.

Rough pipeline:

```
SymbolicDag  ──┐
               ├──▶ [placement]  ──▶  annotated DAG
PlacementHint ─┘                       │
                                       ▼
                                  [edge splitter]
                                       │
                                       ▼
                                  [slot allocator]
                                       │
                                       ▼
                                    ClusterDag
                                       │
                                       ▼
                          NodeAgent coordinator.submit()
```

### Stages

1. **Placement** — decide `node_id` for every logical node. Simplest version:
   author tags each node with a `node_id` field; Partitioner just validates.
   Later: cost model over (compute, data locality, link bandwidth).

2. **Edge splitter** — for every edge whose endpoints are on different nodes,
   replace it with a `(RemoteSend, RemoteRecv)` pair. Pick matching slot IDs
   on both sides. Insert any required barriers / synchronization.

3. **Slot allocator** — assign stream/IO slot numbers consistently across the
   partition. Reuse slot IDs for disjoint lifetimes where safe.

4. **MR2 sizing and opt-in** *(future; interacts with the on-demand MR2 feature
   in the Executor)* — the Executor's MR2 feature is opt-in per transfer via
   a `use_mr2` flag on `RemoteSend` / `RemoteRecv`, paired with an explicit
   `RdmaReserve { size, peer }` node on the receiver side. Both are
   author-written today. A Partitioner should derive them automatically from
   an edge annotation such as `expected_bytes: 100 MiB`: emit
   `RdmaReserve` on the receiver, set `use_mr2: true` on both halves of the
   `Remote*` pair.

## Open questions

- **Representation**: a separate Rust crate producing ClusterDag JSON, or an
  in-process preprocessing step inside NodeAgent's submit path?
- **Placement input**: author-annotated vs. cost-model-driven. Author
  annotation is the simpler MVP and covers the research workloads.
- **Shape of edges**: today cross-node edges are always `RemoteSend/Recv`.
  Should the Partitioner also know about RDMA atomics, broadcast trees, or
  ring-reduce patterns for collective ops?
- **Round-trip**: do we persist the partitioned ClusterDag (authoring artifact)
  or re-partition at every submit (single source of truth)?
- **Interaction with MR2 reservation**: the on-demand MR2 DAG node
  (`RdmaReserve`) is author-written today. A Partitioner should be able to
  derive it automatically from an edge annotation like `expected_bytes: 100 MiB`.

## Not in scope (yet)

- Dynamic re-partitioning during a running DAG
- Failure recovery / node failover
- Cross-cluster federation

---

Revisit this once the on-demand MR2 feature in the Executor has shipped and we
have more than a handful of ClusterDag workloads authored by hand.
