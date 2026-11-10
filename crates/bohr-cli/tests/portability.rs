//! Cross-machine reproduction: a bundle produced on one machine replays on another.
//!
//! CI runs the two halves on different machines (goal T4): `produce_*` writes bundles into a
//! directory and `verify_*` replays a directory it is given.
//!
//! Built-in guests run anywhere; traced bundles are produced only where a guest can be traced.

use std::{path::PathBuf, process::Command};

/// Seeds every producing run covers; three, so one lucky seed cannot carry the claim.
const SEEDS: [&str; 3] = ["0", "42", "7"];

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary must be runnable");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The checkout root, where relative paths in the environment start.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Where produced bundles go, and where a verifying run looks by default.
fn output_dir() -> PathBuf {
    std::env::var("BOHR_PORTABILITY_BUNDLES")
        .map_or_else(|_| repo_root().join("target/bohr-out/portability"), |named| resolve(&named))
}

/// A path from the environment, taken as given if absolute and against the checkout if not.
fn resolve(named: &str) -> PathBuf {
    let path = PathBuf::from(named);
    if path.is_absolute() { path } else { repo_root().join(path) }
}

/// A guest this build can trace: built, and on a host with a backend.
fn traceable_guest(name: &str) -> Option<String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return None;
    }
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/guests/build").join(name);
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

#[test]
fn produce_bundles_for_cross_machine_replay() {
    let dir = output_dir();
    std::fs::create_dir_all(&dir).expect("output directory");

    for seed in SEEDS {
        let into = dir.join(format!("builtin-seed{seed}"));
        std::fs::create_dir_all(&into).expect("seed directory");
        let (code, report) = bohr(&[
            "selftest",
            "--seed",
            seed,
            "--repeat",
            "1",
            "--save-dir",
            &into.to_string_lossy(),
        ]);
        assert_eq!(code, 0, "producing at seed {seed}: {report}");
    }

    // A traced bundle carries a real syscall stream, the likeliest to differ across processors.
    // The guest travels with it: another machine builds its own copy with another compiler, and a
    // replay refuses a binary whose bytes differ from the recorded ones.
    if let Some(path) = traceable_guest("clock_and_random") {
        let saved = dir.join("traced-clock_and_random.bohr");
        let (code, report) =
            bohr(&["run", "--seed", "42", "--save", &saved.to_string_lossy(), "--", &path]);
        assert!(code == 0 || code == 4, "producing a traced bundle: {report}");
        std::fs::create_dir_all(dir.join("bin")).expect("binaries directory");
        std::fs::copy(&path, dir.join("bin/clock_and_random")).expect("the guest travels");
    }
}

#[test]
fn verify_bundles_replay_to_what_they_recorded() {
    // Only a named directory, so the verdict never depends on the producing half of this run.
    let Ok(named) = std::env::var("BOHR_PORTABILITY_BUNDLES") else {
        eprintln!("BOHR_PORTABILITY_BUNDLES names no directory; nothing to verify here");
        return;
    };
    let dir = resolve(&named);
    let bundles = collect(&dir);
    // An empty named directory fails: the bundles never arrived.
    assert!(!bundles.is_empty(), "no bundles under {}; none were verified", dir.display());

    for bundle in &bundles {
        let (code, report) = replay_beside_its_binaries(bundle);
        assert_eq!(code, 0, "{bundle} did not replay: {report}");
    }
    eprintln!("{} bundles replayed", bundles.len());
}

/// Replays a bundle with the binaries that travelled beside it, where there are any.
///
/// Run from the bundle's directory with `--binaries bin`, so the moved path is shorter than the
/// recorded one and the replay can keep the recorded stack (`docs/API.md` §7.3).
fn replay_beside_its_binaries(bundle: &str) -> (i32, String) {
    let path = std::path::Path::new(bundle);
    let Some(home) = path.parent().filter(|dir| dir.join("bin").is_dir()) else {
        return bohr(&["replay", "--verify-only", bundle]);
    };
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(["replay", "--verify-only", "--binaries", "bin"])
        .arg(path.file_name().expect("a bundle has a file name"))
        .current_dir(home)
        .output()
        .expect("the binary must be runnable");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Every bundle under a directory, at any depth.
fn collect(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect(&path));
        } else if path.extension().is_some_and(|e| e == "bohr") {
            out.push(path.to_string_lossy().into_owned());
        }
    }
    // Sorted, so logs from two machines diff cleanly.
    out.sort();
    out
}
