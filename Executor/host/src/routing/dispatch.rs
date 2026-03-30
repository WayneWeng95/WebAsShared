// Host-side slice dispatcher.
//
// Routes any `DispatchSlice` implementation to parallel workers using a
// round-robin assignment strategy, then executes all workers concurrently
// inside a `thread::scope`.
//
// Two built-in slice types are provided:
//   - `FileSlice<'a>`  — zero-copy view into a memory-mapped `MappedFile`
//   - `OwnedSlice`     — worker-produced data held in a `Vec<u8>`
//
// Custom slice types can be dispatched by implementing `DispatchSlice`.
//
// Usage (file-backed):
//   let loaded  = mmap_file(Path::new("/data/input.csv"))?;
//   let slicer  = Slicer::new(&loaded);
//   let slices  = slicer.slice(&LineBoundarySlice, 8);
//
//   FileDispatcher::new(8).run(slices, |assignment| {
//       for s in &assignment.slices { process(s.data()); }
//   });
//
// Usage (worker-owned):
//   let slices: Vec<OwnedSlice> = results
//       .into_iter()
//       .enumerate()
//       .map(|(i, v)| OwnedSlice { index: i, data: v })
//       .collect();
//
//   FileDispatcher::new(4).run(slices, |assignment| {
//       for s in &assignment.slices { send_to_shm(s.data()); }
//   });

use std::thread;
use crate::runtime::mem_operation::slicer::FileSlice;

// -----------------------------------------------------------------------------
// DispatchSlice trait
// -----------------------------------------------------------------------------

/// A unit of work that can be distributed by `FileDispatcher`.
///
/// Implementors provide an ordinal index (used for round-robin placement) and
/// a byte-slice view of their payload.
pub trait DispatchSlice: Send {
    /// Ordinal index of this slice, used for round-robin worker assignment.
    fn index(&self) -> usize;
    /// The byte payload to be processed.
    fn data(&self) -> &[u8];
    /// Byte length of the payload.
    fn len(&self) -> usize {
        self.data().len()
    }
    /// Returns `true` if the payload is empty.
    fn is_empty(&self) -> bool {
        self.data().is_empty()
    }
}

// -----------------------------------------------------------------------------
// FileSlice impl
// -----------------------------------------------------------------------------

impl<'a> DispatchSlice for FileSlice<'a> {
    fn index(&self) -> usize { self.index }
    fn data(&self) -> &[u8] { self.data }
}

// -----------------------------------------------------------------------------
// OwnedSlice
// -----------------------------------------------------------------------------

/// A worker-produced byte payload with an ordinal index.
///
/// Use this when a worker computes results in memory and needs to dispatch
/// them to further workers without going through a memory-mapped file.
///
/// # Example
/// ```
/// let slice = OwnedSlice { index: 0, data: b"hello world".to_vec() };
/// assert_eq!(slice.data(), b"hello world");
/// ```
pub struct OwnedSlice {
    /// Ordinal index for round-robin placement.
    pub index: usize,
    /// The owned payload.
    pub data: Vec<u8>,
}

impl DispatchSlice for OwnedSlice {
    fn index(&self) -> usize { self.index }
    fn data(&self) -> &[u8] { &self.data }
}

// -----------------------------------------------------------------------------
// WorkerAssignment
// -----------------------------------------------------------------------------

/// A set of slices assigned to one logical worker.
///
/// A single worker may receive multiple non-contiguous slices when the number
/// of slices produced by the policy exceeds the worker count.
pub struct WorkerAssignment<S> {
    /// Zero-based worker index in `[0, worker_count)`.
    pub worker_id: usize,
    /// The slices assigned to this worker, in the order they were dispatched.
    pub slices: Vec<S>,
}

impl<S: DispatchSlice> WorkerAssignment<S> {
    /// Total number of bytes assigned to this worker across all its slices.
    pub fn total_bytes(&self) -> usize {
        self.slices.iter().map(|s| s.len()).sum()
    }

    /// Number of slices assigned to this worker.
    pub fn slice_count(&self) -> usize {
        self.slices.len()
    }
}

impl<S: DispatchSlice> std::fmt::Debug for WorkerAssignment<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WorkerAssignment {{ worker_id: {}, slices: {}, bytes: {} }}",
            self.worker_id,
            self.slices.len(),
            self.total_bytes()
        )
    }
}

// -----------------------------------------------------------------------------
// FileDispatcher
// -----------------------------------------------------------------------------

/// Assigns slices to workers and executes them in parallel.
///
/// Slices are assigned to workers round-robin by slice index:
///   slice 0 → worker 0, slice 1 → worker 1, …,
///   slice N → worker (N % worker_count), …
///
/// This distributes slices evenly when the slice count is a multiple of the
/// worker count, and leaves at most `worker_count - 1` workers with one
/// extra slice otherwise.  It mirrors the call-order assignment of
/// `RoundRobinPartition` in the shuffle layer.
///
/// Works with any type implementing `DispatchSlice`: `FileSlice<'a>` for
/// zero-copy file dispatch and `OwnedSlice` for worker-produced data.
pub struct FileDispatcher {
    worker_count: usize,
}

impl FileDispatcher {
    /// Create a dispatcher that will spread work across `worker_count` workers.
    ///
    /// # Panics
    /// Panics if `worker_count == 0`.
    pub fn new(worker_count: usize) -> Self {
        assert!(worker_count > 0, "worker_count must be > 0");
        Self { worker_count }
    }

    /// Assign `slices` to workers round-robin, returning one `WorkerAssignment`
    /// per worker that received at least one slice.
    ///
    /// Workers that receive no slices (when `slices.len() < worker_count`) are
    /// omitted from the result.
    pub fn assign<S: DispatchSlice>(&self, slices: Vec<S>) -> Vec<WorkerAssignment<S>> {
        let n = self.worker_count;
        let mut buckets: Vec<Vec<S>> = (0..n).map(|_| Vec::new()).collect();

        for slice in slices {
            buckets[slice.index() % n].push(slice);
        }

        buckets
            .into_iter()
            .enumerate()
            .filter(|(_, b)| !b.is_empty())
            .map(|(worker_id, slices)| WorkerAssignment { worker_id, slices })
            .collect()
    }

    /// Assign slices and execute `worker_fn` for each assignment concurrently.
    ///
    /// All worker threads are spawned inside a `thread::scope` so they are
    /// guaranteed to finish before this function returns.
    ///
    /// Workers that receive no slices are skipped; the closure is only called
    /// for workers with at least one slice.
    ///
    /// # Example
    /// ```no_run
    /// dispatcher.run(slices, |assignment| {
    ///     for slice in &assignment.slices {
    ///         process(slice.data());
    ///     }
    /// });
    /// ```
    pub fn run<S, F>(&self, slices: Vec<S>, worker_fn: F)
    where
        S: DispatchSlice + Sync,
        F: Fn(&WorkerAssignment<S>) + Send + Sync,
    {
        let assignments = self.assign(slices);

        println!(
            "[Dispatcher] {} workers, {} assignments ({} total bytes)",
            self.worker_count,
            assignments.len(),
            assignments.iter().map(|a| a.total_bytes()).sum::<usize>(),
        );

        thread::scope(|s| {
            for assignment in &assignments {
                s.spawn(|| {
                    println!(
                        "[Dispatcher] Worker {} starting ({} slices, {} bytes)",
                        assignment.worker_id,
                        assignment.slice_count(),
                        assignment.total_bytes(),
                    );
                    worker_fn(assignment);
                    println!("[Dispatcher] Worker {} done", assignment.worker_id);
                });
            }
        });
    }
}
