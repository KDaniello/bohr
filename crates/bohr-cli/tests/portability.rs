//! Cross-machine reproduction: a bundle produced on one machine replays on another.
//!
//! Split into two halves by test name, because continuous integration runs them on different
//! machines: `produce_*` writes bundles into an output directory, `verify_*` replays whatever it
//! is pointed at. One machine produces, another verifies, and neither knows which processor the
//! other had — which is the whole claim (goal T4).
//!
//! Bundles from the built-in guests need no privileges and no Linux, so this runs anywhere; the
//! traced ones are produced where they can be and skipped where they cannot.

use std::{path::PathBuf, process::Command};

/// Seeds every producing run covers. Three rather than one: a single seed that happened to avoid
/// a divergent decision would make the whole cross-machine claim rest on luck.
const SEEDS: [&str; 3] = ["0", "42", "7"];

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary must be runnable");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The root of the checkout, which is what a relative path in the environment means.
///
/// A test runs with its own crate as the working directory, so `BOHR_PORTABILITY_BUNDLES=bundles`
/// would otherwise point three directories away from where the caller put the bundles — and this
/// half of the test would then find nothing to verify, which is not a thing to discover from a
/// green run.
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

/// A guest this build could actually trace: present on disk, and on a host with a backend.
///
/// The file being there is not enough — a guest built under Linux is visible from a Windows
/// checkout of the same tree, and running it there is a refusal rather than a bundle.
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

    // A traced guest as well, where one can be traced: it is the bundle that carries a real
    // syscall stream, and the one a different processor is most likely to disagree about.
    if let Some(path) = traceable_guest("clock_and_random") {
        let saved = dir.join("traced-clock_and_random.bohr");
        let (code, report) =
            bohr(&["run", "--seed", "42", "--save", &saved.to_string_lossy(), "--", &path]);
        assert!(code == 0 || code == 4, "producing a traced bundle: {report}");
    }
}

#[test]
fn verify_bundles_replay_to_what_they_recorded() {
    // Only where a directory was named. The two halves are meant for two machines, and reading
    // whatever the producing half of this same run happens to have written so far would make the
    // verdict depend on which test finished first.
    let Ok(named) = std::env::var("BOHR_PORTABILITY_BUNDLES") else {
        eprintln!("BOHR_PORTABILITY_BUNDLES names no directory; nothing to verify here");
        return;
    };
    let dir = resolve(&named);
    let bundles = collect(&dir);
    // Naming a directory is a claim that bundles are in it. An empty one means they were never
    // produced, or never arrived, and passing on it would turn the cross-machine gate into a job
    // that is green because it checked nothing.
    assert!(!bundles.is_empty(), "no bundles under {}; none were verified", dir.display());

    for bundle in &bundles {
        let (code, report) = bohr(&["replay", "--verify-only", bundle]);
        assert_eq!(code, 0, "{bundle} did not replay: {report}");
    }
    eprintln!("{} bundles replayed", bundles.len());
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
    // Sorted, so two machines report the same order and a diff of two logs is readable.
    out.sort();
    out
}
