mod barrier_test;
mod demos;
mod finra;

/// Unpack a fan-out function's `arg` into `(base_slot, worker_count)`.
///
/// The partitioner packs `arg = base | (n_consumers << 16)` (see
/// `partitioner::slot_assigner`).  Hand-authored single-node DAGs predate the
/// packing and pass a bare worker count with the high bits clear; for those we
/// fall back to `default_base` and treat `arg` as the count.
pub(crate) fn unpack_fanout_arg(arg: u32, default_base: u32) -> (u32, u32) {
    let base = arg & 0xFFFF;
    let n = arg >> 16;
    if n == 0 {
        (default_base, base) // legacy: arg is the worker count
    } else {
        (base, n)
    }
}
mod img_pipeline;
mod matrix;
mod ml_inference;
mod ml_training;
mod routing_tests;
mod stream_pipeline;
mod terasort;
mod tfidf;
mod word_count;
