// Applies a `SlicePolicy` to a memory-mapped `LoadedFile` to produce a set of
// non-overlapping `FileSlice` views ready for parallel dispatch.
//
// The slicer is intentionally stateless beyond holding a reference to the
// loaded file: the same `Slicer` can be re-used with different policies or
// different worker counts without re-reading the file.
//
// Usage:
//   let loaded = load_file(Path::new("/data/input.csv"))?;
//   let slicer = Slicer::new(&loaded);
//   let slices = slicer.slice(&LineBoundarySlice, 8);
//   FileDispatcher::new(8).run(slices, |assignment| { /* process */ });

use crate::policy::SlicePolicy;
use crate::runtime::inputer::LoadedFile;

// -----------------------------------------------------------------------------
// FileSlice — a single view into the loaded file
// -----------------------------------------------------------------------------

/// A read-only view of one chunk of a memory-mapped file.
///
/// The lifetime `'a` is tied to the `LoadedFile` that backs this slice, so
/// the mapping is guaranteed to remain valid for as long as any `FileSlice`
/// derived from it is alive.
#[derive(Clone, Copy)]
pub struct FileSlice<'a> {
    /// Zero-based position of this chunk among all produced slices.
    pub index: usize,
    /// Byte offset of the first byte of this chunk within the original file.
    pub offset: usize,
    /// The actual bytes of this chunk.
    pub data: &'a [u8],
}

impl<'a> FileSlice<'a> {
    /// Number of bytes in this chunk.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl<'a> std::fmt::Debug for FileSlice<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FileSlice {{ index: {}, offset: {}, len: {} }}",
            self.index,
            self.offset,
            self.data.len()
        )
    }
}

// -----------------------------------------------------------------------------
// Slicer
// -----------------------------------------------------------------------------

/// Divides a `LoadedFile` into `FileSlice` views using a `SlicePolicy`.
///
/// The `Slicer` holds a shared reference to the file; the produced slices
/// borrow from the same mapping, so no data is copied.
pub struct Slicer<'a> {
    loaded: &'a LoadedFile,
}

impl<'a> Slicer<'a> {
    /// Create a `Slicer` backed by `loaded`.
    pub fn new(loaded: &'a LoadedFile) -> Self {
        Self { loaded }
    }

    /// Apply `policy` to divide the file into at most `num_workers` chunks.
    ///
    /// The returned slices are ordered by ascending byte offset, contiguous,
    /// and together cover the entire file exactly once.
    ///
    /// The actual number of slices may be less than `num_workers` when the
    /// policy finds fewer natural boundaries (e.g. fewer newlines than
    /// workers for `LineBoundarySlice`).
    ///
    /// # Panics
    /// Panics if `num_workers == 0`.
    pub fn slice<P: SlicePolicy + ?Sized>(&self, policy: &P, num_workers: usize) -> Vec<FileSlice<'a>> {
        assert!(num_workers > 0, "num_workers must be > 0");

        let data = self.loaded.as_bytes();
        let raw = policy.compute_slices(data, num_workers);

        raw.into_iter()
            .enumerate()
            .map(|(index, (offset, len))| FileSlice {
                index,
                offset,
                data: &data[offset..offset + len],
            })
            .collect()
    }
}
