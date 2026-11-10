//! What a run covered, and why a search can follow it.
//!
//! The same seed gives the same map, and a different seed can give a different one.
//!
//! Against the scripted backend, so it runs everywhere. See `docs/API.md` §5.5.

use bohr_testkit::{Fixture, run_once_covered};
use bohr_types::Seed;

/// One seed gives one map.
#[test]
fn the_same_seed_covers_the_same_edges() {
    for seed in [0, 1, 42, 7] {
        let (_, first) = run_once_covered(Fixture::CoinFlips, Seed(seed));
        let (_, again) = run_once_covered(Fixture::CoinFlips, Seed(seed));
        assert_eq!(first.digest(), again.digest(), "seed {seed} covered two different sets");
        assert_eq!(first, again);
    }
}

/// Different seeds give different maps.
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
/// This is why the branching fixture exists.
#[test]
fn a_guest_that_never_branches_covers_one_path_whatever_it_draws() {
    let digests: std::collections::BTreeSet<_> =
        (0..8).map(|s| run_once_covered(Fixture::ClockAndRandom, Seed(s)).1.digest()).collect();

    assert_eq!(digests.len(), 1, "a straight-line guest cannot cover two different sets");
}

/// A guest without coverage has no map, which differs from an empty one.
#[test]
fn a_run_that_happened_is_not_reported_as_a_run_that_covered_nothing() {
    let (report, covered) = run_once_covered(Fixture::CoinFlips, Seed(3));
    assert!(report.events > 0, "the guest must have run for this to say anything");
    assert!(!covered.is_empty(), "a run under a backend that observes execution left no edges");
    assert!(covered.edges() >= 4, "only {} edges for a guest with branches", covered.edges());
}
