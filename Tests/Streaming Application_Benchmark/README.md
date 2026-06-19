# Streaming Application Benchmark

Application-level streaming track: the two RTSFaaS-derived workloads
(**MediaReview**, **SocialNetwork**) compared across **WasMem (ours)** and the
streaming baselines (**RTSFaaS**, **Flink StateFun**). Distinct from the
streaming *feature* tests (`../Streaming/`, `../Streaming_CrossNode/`), which
validate the `StreamPipeline` primitive.

Split by topology, mirroring the suite's `Intra-Node` / `Inter-Node`
application-benchmark folders:

| folder | what | status |
|--------|------|--------|
| [`intra-node/`](intra-node/) | single-machine comparison — WasMem (intra-node SHM) vs RTSFaaS (single-node RoCE) vs Flink StateFun (docker-compose). Workload gen, drivers, baselines, results, figures. | ✅ done |
| [`inter-node/`](inter-node/) | cluster comparison (multi-node RDMA / k8s StateFun / RTSFaaS 5-node + TiKV — its real design point). | 🟡 design/scaffolding — see [`EXPERIMENT_PLAN.md`](inter-node/EXPERIMENT_PLAN.md) |

Start at [`intra-node/README.md`](intra-node/README.md); the headline
three-system result + figure is in
[`intra-node/results_throughput_comparison.md`](intra-node/results_throughput_comparison.md)
(`intra-node/figs/streaming_overlay.png`).
