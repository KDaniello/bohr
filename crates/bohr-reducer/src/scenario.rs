//! The thing being reduced, and what makes one smaller than another.

use bohr_types::Seed;

/// Everything a run of this scenario needs that a reduction is allowed to change.
///
/// The seed is not one of them, and neither is the prefix's length. The tape reads its extension
/// keystream at an offset counted from the end of the prefix, so a prefix of a different length is
/// a different tape rather than an edited one — every byte past the old end would change at once,
/// and a reduction that did that would be comparing two unrelated runs. So the prefix is fixed at
/// the start and only ever overwritten in place.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Scenario {
    seed: Seed,
    prefix: Vec<u8>,
    config: String,
}

impl Scenario {
    /// A scenario as it was found.
    #[must_use]
    pub const fn new(seed: Seed, prefix: Vec<u8>, config: String) -> Self {
        Self { seed, prefix, config }
    }

    /// The seed the tape extends from.
    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.seed
    }

    /// The bytes the run reads before the keystream.
    #[must_use]
    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    /// The world configuration, as the text a bundle stores and a replay re-parses.
    #[must_use]
    pub fn config(&self) -> &str {
        &self.config
    }

    /// The same scenario with a different configuration, which is what removing an entry produces.
    #[must_use]
    pub fn with_config(&self, config: String) -> Self {
        Self { seed: self.seed, prefix: self.prefix.clone(), config }
    }

    /// The same scenario with some prefix bytes overwritten.
    ///
    /// # Panics
    ///
    /// Panics if an edit falls outside the prefix. Every edit is resolved from a journal the run
    /// itself produced against the prefix that produced it, so one that does not fit is a defect
    /// in the resolution rather than a scenario to try.
    #[must_use]
    pub fn with_bytes(&self, edits: &[Edit]) -> Self {
        let mut prefix = self.prefix.clone();
        for edit in edits {
            let at = edit.at as usize;
            let end = at + edit.bytes.len();
            assert!(end <= prefix.len(), "an edit at {at} runs past a prefix of {}", prefix.len());
            prefix[at..end].copy_from_slice(&edit.bytes);
        }
        Self { seed: self.seed, prefix, config: self.config.clone() }
    }
}

/// Bytes to write over one decision.
///
/// Never an insertion and never a removal: the draw ABI promises that what a draw consumes is a
/// function of the request and not of the value, so overwriting one draw's bytes changes exactly
/// that decision and leaves every later one reading from the same offset. Moving a byte would
/// break the property from the other side.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Edit {
    /// Offset into the prefix.
    pub at: u64,
    /// What to write there.
    pub bytes: Vec<u8>,
}

/// How large a run is, in the two units a reduction reports.
///
/// Ordered by events first because that is the number a scenario is judged small by, and by draws
/// second because two runs of one length are not equally simple — the one that made fewer
/// decisions is the one with less left to explain.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Size {
    /// Events recorded.
    pub events: u64,
    /// Draws taken from the tape.
    pub draws: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario() -> Scenario {
        Scenario::new(Seed(1), vec![0xAA; 8], "[network]\n".to_owned())
    }

    #[test]
    fn an_edit_overwrites_and_never_moves() {
        let edited = scenario().with_bytes(&[Edit { at: 2, bytes: vec![0, 0] }]);
        assert_eq!(edited.prefix(), &[0xAA, 0xAA, 0, 0, 0xAA, 0xAA, 0xAA, 0xAA]);
        assert_eq!(edited.prefix().len(), scenario().prefix().len(), "the prefix changed length");
        assert_eq!(edited.seed().get(), scenario().seed().get());
    }

    #[test]
    #[should_panic(expected = "runs past a prefix")]
    fn an_edit_outside_the_prefix_is_a_defect_rather_than_a_candidate() {
        let _ = scenario().with_bytes(&[Edit { at: 7, bytes: vec![0, 0] }]);
    }

    /// Events decide, and draws break the tie. A scenario that records as much but decides less is
    /// the better one to hand somebody.
    #[test]
    fn fewer_events_wins_and_fewer_draws_breaks_the_tie() {
        let small = Size { events: 18, draws: 400 };
        let same_shorter = Size { events: 18, draws: 41 };
        let large = Size { events: 40551, draws: 41 };

        assert!(same_shorter < small);
        assert!(small < large);
        assert!(same_shorter < large);
    }
}
