//! The coverage channel, against a guest that was really built with it.
//!
//! The synthetic half of this claim is checked everywhere by `bohr-testkit`; what can only be
//! checked here is that the descriptor survives the six namespaces and the `execve` a guest is put
//! through, that an instrumented binary finds it, and that what it writes is the same on every run
//! of a seed.
//!
//! Driven through the command line and read back out of the bundle, which is both the path a user
//! takes and the only way to run two traced guests at once: a traced guest is a per-process affair,
//! and two of them inside one test binary would wait on each other's children.
//!
//! Skipped where no guest can be intercepted or the guests have not been built. A check that
//! cannot run must not look like a check that passed, so it says which it was.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{path::PathBuf, process::Command};

use bohr_trace::{Bundle, SectionId};

/// A guest built by `make guests`, if it is there.
fn guest(name: &str) -> Option<String> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/guests/build").join(name);
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// Runs a guest under a seed and returns what its bundle recorded as covered.
///
/// `None` means the bundle carries no coverage section, which is what a guest built without the
/// runtime leaves behind.
fn covered(tag: &str, path: &str, seed: u64) -> Option<Vec<u8>> {
    covered_with(tag, path, seed, None)
}

/// The same, under a scenario that sizes the map.
fn covered_with(tag: &str, path: &str, seed: u64, cells: Option<usize>) -> Option<Vec<u8>> {
    run_under(tag, path, seed, cells).0
}

/// The same, answering what the run covered and which run it was.
fn run_under(tag: &str, path: &str, seed: u64, cells: Option<usize>) -> (Option<Vec<u8>>, String) {
    // Named after the test as well as the seed: these run in parallel, and two of them clearing
    // one directory would fail on each other's evidence rather than on the thing under test.
    let dir = std::env::temp_dir().join(format!("bohr-cov-{tag}-{seed}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory of this test's own");
    let bundle = dir.join("run.bohr");

    let mut args = vec![
        "run".to_owned(),
        "--seed".to_owned(),
        seed.to_string(),
        "--scheduler".to_owned(),
        "random".to_owned(),
        "--save".to_owned(),
        bundle.to_string_lossy().into_owned(),
    ];
    if let Some(cells) = cells {
        let scenario = dir.join("scenario.toml");
        std::fs::write(
            &scenario,
            format!(
                "[coverage]
shm_size = {cells}
"
            ),
        )
        .expect("a scenario of this test's own");
        args.push("--config".to_owned());
        args.push(scenario.to_string_lossy().into_owned());
    }
    args.push("--".to_owned());
    args.push(path.to_owned());

    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(&args)
        .output()
        .expect("the binary under test must be runnable");
    assert!(
        matches!(out.status.code(), Some(0 | 4)),
        "the run did not complete: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let said = String::from_utf8_lossy(&out.stderr);
    let digest = said
        .split_whitespace()
        .find(|word| word.starts_with("blake3:"))
        .unwrap_or_default()
        .to_owned();

    let bytes = std::fs::read(&bundle).expect("the run saved a bundle");
    let parsed = Bundle::parse(&bytes).expect("the bundle it saved is readable");
    (parsed.section(SectionId::Coverage).map(<[u8]>::to_vec), digest)
}

/// The descriptor has to cross the isolation to be of any use, and a guest that found nothing to
/// write to leaves exactly the bytes of a guest that covered nothing.
#[test]
fn an_instrumented_guest_fills_the_map_through_its_isolation() {
    let Some(path) = guest("covered_branches") else {
        eprintln!("covered_branches is not built; nothing to cover");
        return;
    };
    let map = covered("reaches", &path, 1).expect("an instrumented guest records a section");
    assert!(map.iter().any(|c| *c != 0), "the channel did not reach the guest");
}

/// The gate: coverage is as reproducible as the run that produced it. Anything else would make a
/// search irreproducible while every run inside it reproduced perfectly.
#[test]
fn the_same_seed_covers_the_same_edges() {
    let Some(path) = guest("covered_branches") else {
        eprintln!("covered_branches is not built; nothing to cover");
        return;
    };
    for seed in [0, 1, 7] {
        let first = covered("repeats", &path, seed).expect("a coverage section");
        let again = covered("repeats", &path, seed).expect("a coverage section");
        assert_eq!(first, again, "seed {seed} covered two different sets of edges");
    }
}

/// And it still has to move, or there is nothing for a search to follow.
#[test]
fn a_guest_that_branches_on_the_tape_covers_differently_under_different_seeds() {
    let Some(path) = guest("covered_branches") else {
        eprintln!("covered_branches is not built; nothing to cover");
        return;
    };
    let seen: std::collections::BTreeSet<_> =
        (0..8).filter_map(|s| covered("varies", &path, s)).collect();
    assert!(seen.len() >= 2, "eight seeds walked one path through a guest with four");
}

/// A guest built without the runtime is a guest searched without coverage. Recording that as "it
/// covered nothing" would tell a campaign the program does nothing, which is the opposite fact.
#[test]
fn a_guest_without_the_runtime_records_no_coverage_rather_than_empty_coverage() {
    let Some(path) = guest("clock_and_random") else {
        eprintln!("clock_and_random is not built");
        return;
    };
    assert!(
        covered("absent", &path, 1).is_none(),
        "an uninstrumented guest carried a coverage section it cannot have filled"
    );
}

/// The claim: the map is the size the scenario says, and the guest fills that size.
///
/// **A map smaller than the guest has edges stops telling paths apart**, and against a real
/// execution client the default is smaller — measured, thirty-two thousand of sixty-five thousand
/// counters from one startup, after which nearly every run looks novel and the corpus keeps them
/// all. The remedy is a number, so the number has to reach both sides: the supervisor maps it and
/// the guest folds into it, and a guest folding into a width nobody mapped would write past the
/// end of the region.
///
/// Stated as a pair of sizes rather than one, because a section of the default length would
/// satisfy any claim made about a scenario that asked for the default.
#[test]
fn the_map_is_the_size_the_scenario_asked_for() {
    let Some(path) = guest("covered_branches") else {
        eprintln!("covered_branches is not built; nothing to cover");
        return;
    };
    let narrow = covered_with("sized", &path, 1, Some(4096)).expect("a coverage section");
    assert_eq!(narrow.len(), 4096, "the map was not the size the scenario asked for");
    assert!(narrow.iter().any(|c| *c != 0), "a map of that size was never written into");

    let wide = covered_with("sized", &path, 1, Some(262_144)).expect("a coverage section");
    assert_eq!(wide.len(), 262_144);
    assert!(wide.iter().any(|c| *c != 0), "a wider map was never written into");
}

/// The claim: how large the map is is not part of which run this was.
///
/// **Coverage is what a search reads, never what a run does.** The size therefore travels in the
/// shared region's own header and not in the guest's environment — in the environment it is part of
/// what the program was started with, and the same seed under two map sizes digests as two
/// different runs. Which would mean a bundle recorded by one campaign could not be replayed by a
/// campaign searching with a different map, and `docs/API.md` §5.1 does not exempt the explorer.
///
/// It was in the environment for about an hour of one afternoon, which is why it is a test.
///
/// **What this does not claim** is that no guest can tell: against a real execution client the two
/// sizes produce the same events in the same order with the same returns and still digest apart,
/// because a syscall event records six argument registers whatever the call takes and a different
/// memory footprint moves the pointers left in them. That is a defect in what a digest is made of
/// rather than in this channel, and it is recorded in `benches/baseline.toml`.
#[test]
fn the_size_of_the_map_is_not_part_of_which_run_it_was() {
    let Some(path) = guest("covered_branches") else {
        eprintln!("covered_branches is not built; nothing to cover");
        return;
    };
    let (_, default) = run_under("unmoved", &path, 5, None);
    let (_, narrow) = run_under("unmoved", &path, 5, Some(4096));
    let (_, wide) = run_under("unmoved", &path, 5, Some(262_144));

    assert!(default.starts_with("blake3:"), "the run reported no digest at all");
    assert_eq!(narrow, default, "a smaller map made it a different run");
    assert_eq!(wide, default, "a larger map made it a different run");
}
