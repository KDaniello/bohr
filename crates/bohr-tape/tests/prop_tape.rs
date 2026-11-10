//! Property and golden tests for the draw ABI (`docs/TESTING.md` §4.2).
//!
//! The reducer and explorer depend on these properties; a break would make mutation non-local
//! without failing any correctness test.

use bohr_tape::{DrawSite, Tape, draw_width_bytes};
use bohr_types::Seed;
use proptest::prelude::*;

/// A tape whose prefix is `0x00, 0x01, … 0xFF`, so consumed bytes are obvious by inspection.
fn counted_tape() -> Tape {
    Tape::with_prefix(Seed(0), (0..=255u8).collect())
}

/// Expected values computed independently from `docs/API.md` §2.4.
///
/// Derived separately from the spec, never from the code.
#[test]
fn golden_vectors_match_an_independent_derivation_of_the_spec() {
    let tape = counted_tape();
    let mut c = tape.cursor();

    assert_eq!(c.draw_range(10, DrawSite::SchedPick).unwrap(), 0);
    assert_eq!(c.position().get(), 1);

    assert_eq!(c.draw_range(10, DrawSite::SchedPick).unwrap(), 1);
    assert_eq!(c.draw_range(200, DrawSite::SchedPick).unwrap(), 2);
    assert_eq!(c.position().get(), 3);

    assert_eq!(c.draw_range(1000, DrawSite::SchedPick).unwrap(), 27, "two bytes, 0x0403 % 1000");
    assert_eq!(c.position().get(), 5);

    assert!(c.draw_bool(1, 2, DrawSite::NetDeliver).unwrap(), "0x0605 < 32768");
    assert_eq!(c.position().get(), 7);

    assert_eq!(c.draw_weighted(&[1, 2, 3], DrawSite::NodeFault).unwrap(), 0);
    assert_eq!(c.position().get(), 11);

    assert_eq!(c.draw_range(1, DrawSite::SchedPick).unwrap(), 0);
    assert_eq!(c.position().get(), 11, "a degenerate choice consumes nothing");

    assert_eq!(c.draw_range(65_537, DrawSite::SchedPick).unwrap(), 3070, "three bytes");
    assert_eq!(c.position().get(), 14);
    // A certainty costs nothing.
    assert!(!c.draw_bool(0, 3, DrawSite::NetDeliver).unwrap());
    assert_eq!(c.position().get(), 14, "an impossibility consumed a byte");
    assert!(c.draw_bool(3, 3, DrawSite::NetDeliver).unwrap());
    assert_eq!(c.position().get(), 14, "a certainty consumed a byte");
    assert_eq!(c.draw_weighted(&[0, 5], DrawSite::NodeFault).unwrap(), 1);
    assert_eq!(c.position().get(), 14, "a single-outcome weighting consumed a byte");
}

proptest! {
    /// The bound the scheduler and every fault decision rely on.
    #[test]
    fn range_is_always_in_bounds(seed: u64, n in 1u64..=u64::MAX, draws in 1usize..32) {
        let tape = Tape::from_seed(Seed(seed));
        let mut c = tape.cursor();
        for _ in 0..draws {
            let v = c.draw_range(n, DrawSite::SchedPick)?;
            prop_assert!(v < n);
        }
    }

    /// A mutated byte changes a decision, never the alignment of later ones.
    #[test]
    fn consumption_is_a_function_of_the_request_alone(seeds in prop::collection::vec(any::<u64>(), 2..12), n in 2u64..=u64::MAX) {
        let expected = draw_width_bytes(n) as u64;
        for seed in seeds {
            let tape = Tape::from_seed(Seed(seed));
            let mut c = tape.cursor();
            c.draw_range(n, DrawSite::SchedPick)?;
            prop_assert_eq!(c.position().get(), expected, "seed {} shifted the layout", seed);
        }
    }

    /// A boolean costs two bytes at any probability except the certainties, which cost nothing
    /// (`docs/API.md` §2.4). Stated over the whole range.
    #[test]
    fn boolean_width_depends_on_the_denominator_only_at_certainty(seed: u64, den in 1u32..=65_536, num in 0u32..=65_536) {
        let tape = Tape::from_seed(Seed(seed));
        let mut c = tape.cursor();
        c.draw_bool(num, den, DrawSite::NetDeliver)?;

        let certain = num == 0 || num >= den;
        prop_assert_eq!(c.position().get(), if certain { 0 } else { 2 },
            "num {} of den {} consumed the wrong width", num, den);
    }

    /// Reads are seekable: a cursor advanced in pieces sees what one long read would have seen.
    #[test]
    fn split_reads_agree_with_one_long_read(seed: u64, prefix in prop::collection::vec(any::<u8>(), 0..40), split in 0usize..48) {
        let tape = Tape::with_prefix(Seed(seed), prefix);
        let total = 48usize;
        let mut whole = vec![0u8; total];
        tape.read_at(0, &mut whole);

        let split = split.min(total);
        let (mut head, mut tail) = (vec![0u8; split], vec![0u8; total - split]);
        tape.read_at(0, &mut head);
        tape.read_at(split as u64, &mut tail);

        prop_assert_eq!(&whole[..split], &head[..]);
        prop_assert_eq!(&whole[split..], &tail[..]);
    }

    /// A damaged container is refused.
    #[test]
    fn container_round_trips_and_detects_single_byte_damage(seed: u64, prefix in prop::collection::vec(any::<u8>(), 0..64), flip in 0usize..1024) {
        let tape = Tape::with_prefix(Seed(seed), prefix);
        let bytes = tape.to_container();
        let back = Tape::from_container(&bytes).expect("a freshly written container must parse");
        prop_assert_eq!(back.prefix(), tape.prefix());
        prop_assert_eq!(back.seed(), tape.seed());

        // Prefix and hash only; header fields have unit tests.
        prop_assume!(!tape.prefix().is_empty());
        let idx = 64 + (flip % tape.prefix().len());
        let mut damaged = bytes.clone();
        damaged[idx] ^= 1 << (flip % 8);
        prop_assert!(Tape::from_container(&damaged).is_err(), "damage at {} went unnoticed", idx);
    }

    /// Two tapes built the same way are the same tape.
    #[test]
    fn the_seed_alone_determines_every_byte(seed: u64) {
        let a = Tape::from_seed(Seed(seed));
        let b = Tape::from_seed(Seed(seed));
        let (mut x, mut y) = ([0u8; 96], [0u8; 96]);
        a.read_at(0, &mut x);
        b.read_at(0, &mut y);
        prop_assert_eq!(x, y);
    }
}
