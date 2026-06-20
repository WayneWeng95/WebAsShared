# Flink StateFun on Kubernetes (Strimzi/Kafka)

`statefun-kafka.yaml` (applied by `../06-strimzi.sh`) creates the Kafka cluster
`kafka-cluster` + 10 topics on a modern Strimzi operator. Then distribute the 3
`streaming-example-*` images (`../07-load-images.sh`) and apply the master + function
deployments from the streaming benchmark
(`../../../Streaming Application_Benchmark/intra-node/baseline/FlinkStateFun/deployment/k8s/`),
skipping its old `kafka.yaml`/`topics.yaml`. See ../README.md for the full sequence.
