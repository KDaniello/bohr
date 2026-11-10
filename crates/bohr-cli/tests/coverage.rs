//! The coverage channel, against a guest that was really built with it.
//!
//! `bohr-testkit` checks the synthetic half. These check that the descriptor survives the
//! namespaces and `execve`, that an instrumented binary finds it, and that a seed writes the same
//! map every run.
//!
//! Each test runs the CLI and reads the bundle, since two traced guests in one test process would
//! wait on each other's children. A test with no built guest says so and returns.

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
/// `None` means no coverage section, as for a guest built without the runtime.
fn covered(tag: &str, path: &str, seed: u64) -> Option<Vec<u8>> {
    covered_with(tag, path, seed, None)
}

/// The same, under a scenario that sizes the map.
fn covered_with(tag: &str, path: &str, seed: u64, cells: Option<usize>) -> Option<Vec<u8>> {
    run_under(tag, path, seed, cells).0
}

/// The same, answering what the run covered and which run it was.
fn run_under(tag: &str, path: &str, seed: u64, cells: Option<usize>) -> (Option<Vec<u8>>, String) {
    // Named by test and seed, since tests run in parallel.
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

/// The descriptor crosses the isolation and the guest writes into it.
#[test]
fn an_instrumented_guest_fills_the_map_through_its_isolation() {
    let Some(path) = guest("covered_branches") else {
        eprintln!("covered_branches is not built; nothing to cover");
        return;
    };
    let map = covered("reaches", &path, 1).expect("an instrumented guest records a section");
    assert!(map.iter().any(|c| *c != 0), "the channel did not reach the guest");
}

/// Coverage is as reproducible as the run, so a search is too.
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

/// Different seeds cover differently, which gives a search something to follow.
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

/// A guest without the runtime records no section; an empty one would mean it covered nothing.
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

/// The map is the size the scenario asks for, and the guest fills that size.
///
/// A real execution client sets 32 768 of the default 65 536 counters in one startup, so large
/// targets need a wider map. Two non-default sizes are checked.
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

/// The map size does not change the digest.
///
/// The size travels in the shared region's header, outside the guest's environment, so one seed
/// digests the same under any map. A large guest can still digest apart: syscall events record
/// all six argument registers, and a different footprint moves pointers left in them. That is
/// recorded in `benches/baseline.toml`.
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
