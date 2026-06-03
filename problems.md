The high assignment with dag nodes follows strictly 1,2,3,4 order, if 3 dropped its basically can not make it work.

The dat node numbers field is not fully fixed yet.


----Both can be acceptable just for testing purpose. 


--- The scx seems not connected into the placement, need check. maybe we test with just hard coded policy for easier testing 

--- The testing file size is too small and unable to showcase the fan-out performance benefits, also the measuring need to check

--- bring the comparable frames onto the table


The client pre-partitions (main.rs:222), so it sends a node_dags ClusterDag and the coordinator's resolve_dag uses it as-is. That means all my coordinator-side logic (live-node scaling, SCX clamping, converge) has been dead code — the client partitions blindly with no live-cluster knowledge. Removing total_nodes exposed this: detection broke, and even fixed, the client can't partition without live info.

The fix: client sends the raw SymbolicDag; the coordinator partitions it (which is where live-scaling/SCX/converge live). The partitioner already does Func→WasmVoid for Rust, so no client transform is needed for the symbolic path.


Found the root cause: the client detects SymbolicDag by the literal string "total_nodes" (main.rs:219) — which I just removed from every DAG. So word_count.json now falls through to the "treat as ClusterDag" branch → missing field node_dags.

But there's a deeper issue I need to check: the client partitions client-side (line 222), which means it can't do live-node scaling (the client doesn't know how many nodes are online — only the coordinator does). Let me see how the transform + coordinator path fit together before fixing

