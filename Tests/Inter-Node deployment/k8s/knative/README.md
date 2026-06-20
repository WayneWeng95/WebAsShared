# RMMap on Knative (track C)

Express each app workload as a Knative `Service` per stage wired into a
`Sequence`/`Parallel` flow across nodes; state via Redis **ES protocol only — NO
MITOSIS kernel module** without explicit authorization. Reuse the stage code from
`../../../Intra-Node Application_Benchmark/<W>/baseline/rmmap/`. Knative Serving is
installed by `../03-install-knative.sh`.

TODO: per-stage `service.yaml` + the Sequence/Parallel flow per workload.
