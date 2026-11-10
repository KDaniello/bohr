//! What the guest executed, as an edge histogram.
//!
//! The AFL layout: a fixed table of counters, one per edge of the guest's control-flow graph,
//! incremented as the guest runs. A fixed size means no allocation during the run and no growth a
//! guest could drive.
//!
//! Hit counts are compared in buckets. 100 iterations instead of 99 is the same behaviour; two
//! instead of one is not. See `docs/API.md` §5.5.

use bohr_types::Digest;

/// Default number of counters in a map.
///
/// A map smaller than the guest's edge count merges edges, and a nearly full map makes almost
/// every run look new. A real execution client filled 32,000 of these 65,536 counters at startup,
/// and every run was kept.
///
/// 64 KiB is AFL's default and suits a library. For a large guest, size the map from its reported
/// edge count (`docs/API.md` §5.5).
pub const DEFAULT_COVERAGE_MAP_BYTES: usize = 65_536;

/// An edge histogram of one run.
///
/// Any length. Comparisons treat the shorter map as zero past its end, so a campaign can grow its
/// map without losing what it accumulated.
#[derive(Clone, PartialEq, Eq)]
pub struct CoverageMap {
    /// One counter per cell, however many the search asked for.
    counts: Box<[u8]>,
}

impl CoverageMap {
    /// A map with nothing in it, of the default size.
    #[must_use]
    pub fn new() -> Self {
        Self::with_bytes(DEFAULT_COVERAGE_MAP_BYTES)
    }

    /// A map with nothing in it, of the size asked for. At least one counter.
    #[must_use]
    pub fn with_bytes(bytes: usize) -> Self {
        Self { counts: vec![0; bytes.max(1)].into_boxed_slice() }
    }

    /// A map from stored bytes, or nothing where there are none.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (!bytes.is_empty()).then(|| Self { counts: bytes.to_vec().into_boxed_slice() })
    }

    /// How many counters this map holds.
    #[must_use]
    pub const fn cells(&self) -> usize {
        self.counts.len()
    }

    /// The counters, as they are stored in a bundle.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.counts
    }

    /// The counters, for whoever fills them.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.counts
    }

    /// How many edges were reached at all.
    #[must_use]
    pub fn edges(&self) -> usize {
        self.counts.iter().filter(|c| **c != 0).count()
    }

    /// Whether nothing was recorded.
    ///
    /// True for a guest built without the coverage runtime, which is not the same as one that ran
    /// nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.iter().all(|c| *c == 0)
    }

    /// Folds another run's counters in, keeping whichever bucket is larger.
    ///
    /// Taking the larger bucket keeps the accumulated map monotonic, so novelty is one comparison.
    pub fn merge(&mut self, other: &Self) {
        if other.counts.len() > self.counts.len() {
            let mut grown = vec![0u8; other.counts.len()];
            grown[..self.counts.len()].copy_from_slice(&self.counts);
            self.counts = grown.into_boxed_slice();
        }
        for (mine, theirs) in self.counts.iter_mut().zip(other.counts.iter()) {
            *mine = (*mine).max(bucket(*theirs));
        }
    }

    /// Whether this run reached a bucket the accumulated map has never held.
    ///
    /// A corpus keeps a run that reached a new edge, or a known edge a different number of times.
    #[must_use]
    pub fn has_new_bits(&self, seen: &Self) -> bool {
        self.counts
            .iter()
            .enumerate()
            .any(|(at, mine)| bucket(*mine) > seen.counts.get(at).copied().unwrap_or(0))
    }

    /// Identity of these counters, for recording what a run covered without carrying the map.
    #[must_use]
    pub fn digest(&self) -> Digest {
        Digest::of(&self.counts)
    }
}

/// The bucket a hit count falls in: 1, 2, 3, 4, 8, 16, 32, 128.
///
/// Powers of two, since loop counts differ meaningfully by orders of magnitude. Monotonic in the
/// count, so [`CoverageMap::merge`] can take a maximum.
///
/// Each bucket is named by a count inside it, as in AFL, so classifying a bucket again returns it
/// unchanged and merging accumulated maps loses nothing.
const fn bucket(count: u8) -> u8 {
    match count {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4..=7 => 4,
        8..=15 => 8,
        16..=31 => 16,
        32..=127 => 32,
        _ => 128,
    }
}

impl Default for CoverageMap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CoverageMap {
    /// Prints a summary; the full counters would bury a panic message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverageMap")
            .field("edges", &self.edges())
            .field("digest", &self.digest())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(edges: &[(usize, u8)]) -> CoverageMap {
        let mut map = CoverageMap::new();
        for (at, count) in edges {
            map.as_bytes_mut()[*at] = *count;
        }
        map
    }

    #[test]
    fn an_empty_map_is_told_from_one_that_covered_nothing() {
        assert!(CoverageMap::new().is_empty());
        assert!(!with(&[(0, 1)]).is_empty());
        assert_eq!(with(&[(3, 9), (700, 1)]).edges(), 2);
    }

    /// A new bucket or edge is new coverage; a different count in the same bucket is not.
    #[test]
    fn a_new_bucket_is_new_coverage_and_a_new_count_alone_is_not() {
        let mut seen = CoverageMap::new();
        seen.merge(&with(&[(10, 5)]));

        assert!(!with(&[(10, 5)]).has_new_bits(&seen), "the same bucket is not news");
        assert!(!with(&[(10, 6)]).has_new_bits(&seen), "5 and 6 are the same bucket");
        assert!(with(&[(10, 40)]).has_new_bits(&seen), "an order of magnitude more is news");
        assert!(with(&[(11, 1)]).has_new_bits(&seen), "an edge never reached is news");
    }

    /// Classifying a bucket again returns it unchanged, so merging accumulated maps loses nothing.
    #[test]
    fn classifying_a_bucket_again_leaves_it_where_it_is() {
        for count in 0..=u8::MAX {
            let once = bucket(count);
            assert_eq!(bucket(once), once, "{count} classified to {once} and then moved");
        }
        // The classification is monotonic.
        for count in 1..=u8::MAX {
            assert!(bucket(count) >= bucket(count - 1), "the classification is not monotone");
        }
    }

    /// Merging gives the same result in any order.
    #[test]
    fn merging_is_order_independent_and_never_loses_ground() {
        let runs = [with(&[(1, 1)]), with(&[(1, 40), (2, 1)]), with(&[(3, 200)])];

        let mut forwards = CoverageMap::new();
        for run in &runs {
            forwards.merge(run);
        }
        let mut backwards = CoverageMap::new();
        for run in runs.iter().rev() {
            backwards.merge(run);
        }
        assert_eq!(forwards, backwards, "the accumulated map must not depend on the order");
        assert_eq!(forwards.edges(), 3);
    }

    #[test]
    fn a_map_survives_being_stored_and_read_back() {
        let map = with(&[(0, 1), (DEFAULT_COVERAGE_MAP_BYTES - 1, 255)]);
        let back = CoverageMap::from_bytes(map.as_bytes()).expect("bytes are a map");
        assert_eq!(map, back);
        assert_eq!(map.digest(), back.digest());
        assert!(CoverageMap::from_bytes(&[]).is_none(), "no bytes are no map");
    }

    /// Maps of different lengths compare as if the shorter ended in zeroes, so a grown map keeps
    /// what the smaller one accumulated.
    #[test]
    fn a_map_and_a_larger_one_compare_as_though_the_shorter_ended_in_zeroes() {
        let mut seen = CoverageMap::with_bytes(4);
        seen.merge(&{
            let mut small = CoverageMap::with_bytes(4);
            small.as_bytes_mut()[2] = 1;
            small
        });

        let mut wide = CoverageMap::with_bytes(16);
        wide.as_bytes_mut()[2] = 1;
        assert!(!wide.has_new_bits(&seen), "a known cell is not news at a greater width");
        wide.as_bytes_mut()[9] = 1;
        assert!(wide.has_new_bits(&seen), "a cell past the shorter map's end is news");

        seen.merge(&wide);
        assert_eq!(seen.cells(), 16, "merging a wider map widens what is accumulated");
        assert_eq!(seen.edges(), 2, "and keeps what the narrow one had");
    }
}
