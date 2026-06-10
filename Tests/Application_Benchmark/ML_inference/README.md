# ML inference (MNIST) — WebAsShared vs RMMap / Cloudburst / Faasm  *(backlog)*

Workload **#6** in [`../../../Benchmarks/workload_selection_table.tex`](../../../Benchmarks/workload_selection_table.tex).
Image-classification inference standardized on the **MNIST** handwritten-digit model across all three
systems — chosen over MobileNet because it is the lightest to deploy and gives a uniform model for an
apples-to-apples comparison (RMMap ships it natively; Cloudburst and Faasm otherwise default to
MobileNet).

Detailed when WordCount (#2) is complete. Sketch:

- **Our side (port needed):** an MNIST-inference guest (load model + batch → predict → output);
  reuse the model/dataset from RMMap's `digital-minist`. Additive guest function only.
- **RMMap (primary, native):** base functions `Benchmarks/RMMap/digital-minist/` (`mnist_model.txt`,
  `functions.py`, `dataset/Digits_{Train,Test}.txt`); `PROTOCOL` transport selection as elsewhere.
- **Cloudburst (port, mobilenet→MNIST):** adapt `Benchmarks/Cloudburst/mobilenet.py` /
  `predserving.py` to the MNIST model over Redis.
- **Faasm (port, tflite→MNIST):** adapt `Benchmarks/Faasm/tflite-inference/` to the MNIST model
  inside WASM.
- **Metrics:** inference throughput (images/s) + latency (median/p95/p99), model/feature transfer
  cost (ser = 0 on our side), peak/billable memory. See [`../README.md`](../README.md).

> **Standardize the model.** All three baselines must run the **same MNIST model** as our side —
> swap Cloudburst's MobileNet and Faasm's generic tflite model for MNIST so the comparison is
> apples-to-apples.
