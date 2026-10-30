//! What a run covered, and why a search can follow it.
//!
//! Coverage is the one signal the explorer steers by, so it is held to the same standard as the
//! trace: the same seed must produce the same map, and a different seed must be able to produce a
//! different one. Neither half is optional. A map that varied between identical runs would make
//! the search irreproducible; a map that never varied would make it blind, and a blind search that
//! reports coverage is worse than one that admits it has none.
//!
//! Runs against the scripted backend, so it is checked on every machine rather than only where a
//! guest can be intercepted. See `docs/API.md` §5.5.

use bohr_testkit::{run_once_covered, Fixture};
use bohr_types::Seed;

/// The founding half: coverage is a fact about the run, and the run is a function of the seed.
#[test]
fn the_same_seed_covers_the_same_edges() {
    for seed in [0, 1, 42, 7] {
        let (_, first) = run_once_covered(Fixture::CoinFlips, Seed(seed));
        let (_, again) = run_once_covered(Fixture::CoinFlips, Seed(seed));
        assert_eq!(first.digest(), again.digest(), "seed {seed} covered two different sets");
        assert_eq!(first, again);
    }
}

/// The other half: the map has to move, or there is nothing for a corpus to prefer.
#[test]
fn a_different_seed_can_cover_different_edges() {
    let digests: std::collections::BTreeSet<_> =
        (0..32).map(|s| run_once_covered(Fixture::CoinFlips, Seed(s)).1.digest()).collect();

    assert!(
        digests.len() >= 2,
        "thirty-two seeds walked one path; a search would have nothing to follow"
    );
}

/// A guest whose control flow ignores what it draws covers the same edges however it is seeded.
///
/// Recorded rather than merely true: it is the reason the fixture above exists, and somebody who
/// deletes it should find out here rather than from a search that stopped working.
#[test]
fn a_guest_that_never_branches_covers_one_path_whatever_it_draws() {
    let digests: std::collections::BTreeSet<_> =
        (0..8).map(|s| run_once_covered(Fixture::ClockAndRandom, Seed(s)).1.digest()).collect();

    assert_eq!(digests.len(), 1, "a straight-line guest cannot cover two different sets");
}

/// An empty map means the guest was searched without coverage. A guest that ran and covered
/// nothing would be the same bytes and the opposite fact, so the distinction is asserted where it
/// is cheapest to state.
#[test]
fn a_run_that_happened_is_not_reported_as_a_run_that_covered_nothing() {
    let (report, covered) = run_once_covered(Fixture::CoinFlips, Seed(3));
    assert!(report.events > 0, "the guest must have run for this to say anything");
    assert!(!covered.is_empty(), "a run under a backend that observes execution left no edges");
    assert!(covered.edges() >= 4, "only {} edges for a guest with branches", covered.edges());
}
