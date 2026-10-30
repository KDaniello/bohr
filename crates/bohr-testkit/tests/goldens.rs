//! Digest goldens for `minimal/1` runs.
//!
//! A refactor of the world or the kernel that is meant to be pure code motion must not move a
//! single digest; these vectors are what proves it. A deliberate behaviour change re-blesses them
//! in the same commit, and the commit message says why.

use bohr_testkit::{run_once, Fixture};
use bohr_types::Seed;

const GOLDEN_PATH: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/traces/minimal.golden");

const SEEDS: [u64; 3] = [0, 1, 42];

fn current() -> Vec<(String, u64, String)> {
    let mut rows = Vec::new();
    for fixture in Fixture::all() {
        for seed in SEEDS {
            let digest = run_once(fixture, Seed(seed)).digest.to_string();
            rows.push((fixture.name().to_owned(), seed, digest));
        }
    }
    rows
}

#[test]
fn minimal_world_digests_match_the_committed_goldens() {
    let text = std::fs::read_to_string(GOLDEN_PATH).expect("goldens are committed");
    let recorded: Vec<&str> =
        text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).collect();
    let live = current();
    assert_eq!(recorded.len(), live.len(), "the golden file and the fixture set disagree in size");

    for (line, (fixture, seed, digest)) in recorded.iter().zip(&live) {
        let expected = format!("{fixture} {seed} {digest}");
        assert_eq!(*line, expected, "a minimal/1 digest moved; re-bless only with a stated reason");
    }
}

/// Rewrites the golden file from the current build. Run deliberately, never in CI:
/// `cargo test -p bohr-testkit --test goldens -- --ignored bless`
#[test]
#[ignore = "regenerates the goldens; run only to bless an intended change"]
fn bless() {
    let mut out = String::from(
        "# minimal/1 digest goldens: fixture seed digest\n\
         # Regenerate with: cargo test -p bohr-testkit --test goldens -- --ignored bless\n",
    );
    for (fixture, seed, digest) in current() {
        out.push_str(&format!("{fixture} {seed} {digest}\n"));
    }
    std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/traces"))
        .expect("create testdata/traces");
    std::fs::write(GOLDEN_PATH, out).expect("write goldens");
}
