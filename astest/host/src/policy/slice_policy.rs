// Host-side slice partitioning policies.
//
// A `SlicePolicy` determines how a contiguous byte buffer (a memory-mapped
// input file) is divided into a set of chunks for parallel dispatch.
//
// Policies receive the raw bytes so that content-aware implementations (e.g.
// `LineBoundarySlice`) can inspect the data to find natural cut points.
// Content-agnostic policies (`EqualSlice`, `FixedSizeSlice`) only use `len`.
//
// Usage:
//   let slicer = Slicer::new(&loaded_file);
//   let slices = slicer.slice(&LineBoundarySlice, num_workers);

// -----------------------------------------------------------------------------
// Trait
// -----------------------------------------------------------------------------

/// Decides how a buffer of `data.len()` bytes is divided into `num_workers`
/// chunks.
///
/// Returns a `Vec<(offset, length)>` covering the entire buffer without gaps
/// or overlaps.  The number of returned entries may differ from `num_workers`
/// when the policy rounds to natural boundaries or when `num_workers` exceeds
/// the number of available split points.
pub trait SlicePolicy: Send + Sync {
    fn compute_slices(&self, data: &[u8], num_workers: usize) -> Vec<(usize, usize)>;
}

// -----------------------------------------------------------------------------
// EqualSlice
// -----------------------------------------------------------------------------

/// Divides the buffer into `num_workers` equal-sized chunks.
///
/// The last chunk absorbs any remainder so the entire buffer is always covered.
/// If `num_workers > data.len()` the result is capped at one byte per chunk.
///
/// This is the fastest policy: O(1) regardless of content.
pub struct EqualSlice;

impl SlicePolicy for EqualSlice {
    fn compute_slices(&self, data: &[u8], num_workers: usize) -> Vec<(usize, usize)> {
        let len = data.len();
        if len == 0 || num_workers == 0 {
            return vec![];
        }

        // Never produce empty chunks.
        let n = num_workers.min(len);
        let base = len / n;
        let remainder = len % n;

        let mut chunks = Vec::with_capacity(n);
        let mut offset = 0;

        for i in 0..n {
            let extra = usize::from(i < remainder);
            let chunk_len = base + extra;
            chunks.push((offset, chunk_len));
            offset += chunk_len;
        }

        debug_assert_eq!(offset, len);
        chunks
    }
}

// -----------------------------------------------------------------------------
// FixedSizeSlice
// -----------------------------------------------------------------------------

/// Divides the buffer into consecutive chunks of at most `max_bytes` each.
///
/// The number of chunks is `ceil(len / max_bytes)` — independent of
/// `num_workers`.  The last chunk absorbs any remainder.
///
/// Useful when downstream workers have a fixed processing capacity and must
/// not receive more than a certain amount of data at once.
pub struct FixedSizeSlice {
    /// Maximum bytes per chunk.
    pub max_bytes: usize,
}

impl FixedSizeSlice {
    pub fn new(max_bytes: usize) -> Self {
        assert!(max_bytes > 0, "max_bytes must be > 0");
        Self { max_bytes }
    }
}

impl SlicePolicy for FixedSizeSlice {
    fn compute_slices(&self, data: &[u8], _num_workers: usize) -> Vec<(usize, usize)> {
        let len = data.len();
        if len == 0 {
            return vec![];
        }

        let n = len.div_ceil(self.max_bytes);
        let mut chunks = Vec::with_capacity(n);
        let mut offset = 0;

        while offset < len {
            let chunk_len = self.max_bytes.min(len - offset);
            chunks.push((offset, chunk_len));
            offset += chunk_len;
        }

        chunks
    }
}

// -----------------------------------------------------------------------------
// LineBoundarySlice
// -----------------------------------------------------------------------------

/// Divides the buffer into `num_workers` chunks aligned to newline (`\n`)
/// boundaries so that no line is split across two workers.
///
/// Algorithm:
///   1. Compute `num_workers` equally-spaced byte targets.
///   2. For each target, advance to the first `\n` at-or-after it to find the
///      actual cut point.
///   3. Deduplicate cut points that fall on the same newline.
///
/// The result may have fewer than `num_workers` entries when the file contains
/// fewer lines than workers.  Falls back to a single chunk covering the entire
/// buffer if no newline is present.
///
/// Time complexity: O(len) — each byte is visited at most once across all scans.
pub struct LineBoundarySlice;

impl SlicePolicy for LineBoundarySlice {
    fn compute_slices(&self, data: &[u8], num_workers: usize) -> Vec<(usize, usize)> {
        let len = data.len();
        if len == 0 || num_workers == 0 {
            return vec![];
        }
        if num_workers == 1 {
            return vec![(0, len)];
        }

        // Collect unique chunk-start positions.
        // We always start from 0; a sentinel `len` is pushed at the end.
        let mut starts: Vec<usize> = Vec::with_capacity(num_workers + 1);
        starts.push(0);

        for i in 1..num_workers {
            let target = i * len / num_workers;

            // Advance to the byte just after the next newline at-or-after target.
            // This ensures the boundary falls between lines, not inside one.
            let cut = match data[target..].iter().position(|&b| b == b'\n') {
                Some(rel) => target + rel + 1, // character after the newline
                None => len,                   // no newline found; remainder is one chunk
            };

            // Only register the cut if it actually advances past the previous
            // start, and doesn't exceed the file length.
            if cut > *starts.last().unwrap() && cut < len {
                starts.push(cut);
            }
        }
        starts.push(len); // sentinel

        // Convert starts into (offset, length) pairs.
        starts
            .windows(2)
            .filter(|w| w[1] > w[0])
            .map(|w| (w[0], w[1] - w[0]))
            .collect()
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn total(chunks: &[(usize, usize)]) -> usize {
        chunks.iter().map(|&(_, l)| l).sum()
    }

    #[test]
    fn equal_slice_covers_all_bytes() {
        let data = vec![0u8; 100];
        let chunks = EqualSlice.compute_slices(&data, 7);
        assert_eq!(total(&chunks), 100);
    }

    #[test]
    fn equal_slice_caps_at_len() {
        let data = vec![0u8; 3];
        let chunks = EqualSlice.compute_slices(&data, 10);
        assert_eq!(chunks.len(), 3);
        assert_eq!(total(&chunks), 3);
    }

    #[test]
    fn fixed_size_covers_all_bytes() {
        let data = vec![0u8; 103];
        let chunks = FixedSizeSlice::new(10).compute_slices(&data, 99);
        assert_eq!(chunks.len(), 11); // ceil(103/10)
        assert_eq!(total(&chunks), 103);
    }

    #[test]
    fn line_boundary_no_split_mid_line() {
        let data = b"aaa\nbbb\nccc\nddd\n";
        let chunks = LineBoundarySlice.compute_slices(data, 4);
        assert_eq!(total(&chunks), data.len());
        for &(off, _len) in &chunks {
            // Each chunk must start at offset 0 or immediately after a `\n`.
            if off > 0 {
                assert_eq!(data[off - 1], b'\n', "chunk at {off} cuts mid-line");
            }
        }
    }

    #[test]
    fn line_boundary_fewer_lines_than_workers() {
        let data = b"only one line";
        let chunks = LineBoundarySlice.compute_slices(data, 8);
        // No newline → single chunk covering all bytes.
        assert_eq!(chunks.len(), 1);
        assert_eq!(total(&chunks), data.len());
    }
}
