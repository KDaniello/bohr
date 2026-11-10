//! `bohr reduce`: the surface, the refusals, and that what comes out still replays.

use std::{path::PathBuf, process::Command};

fn bohr(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary under test must be runnable");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A directory of this test's own, emptied first.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bohr-reduce-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory of this test's own");
    dir
}

fn stored(name: &str) -> String {
    format!("{}/../../testdata/bundles/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// A clean bundle is refused before any replay.
#[test]
fn a_clean_bundle_is_refused_with_the_reason() {
    let out = scratch("clean").join("smaller.bohr");
    let (code, _, err) =
        bohr(&["reduce", &stored("two-tasks-seed0.bohr"), "-o", &out.to_string_lossy()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-5003"), "the refusal must name its code: {err}");
    assert!(err.contains("nothing to reduce"), "and say what is wrong: {err}");
}

#[test]
fn a_budget_too_small_to_prove_an_answer_is_refused() {
    let out = scratch("budget").join("smaller.bohr");
    let (code, _, err) = bohr(&[
        "reduce",
        &stored("two-tasks-seed0.bohr"),
        "-o",
        &out.to_string_lossy(),
        "--budget",
        "1",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-1001") || err.contains("BOHR-5003"), "{err}");
}

#[test]
fn an_unknown_thing_to_keep_is_refused_and_says_what_there_is() {
    let out = scratch("keep").join("smaller.bohr");
    let (code, _, err) = bohr(&[
        "reduce",
        &stored("two-tasks-seed0.bohr"),
        "-o",
        &out.to_string_lossy(),
        "--keep",
        "everything",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-1001"), "{err}");
    assert!(err.contains("events") && err.contains("tape"), "{err}");
}

#[test]
fn a_bundle_that_is_not_there_is_a_usage_error() {
    let out = scratch("absent").join("smaller.bohr");
    let (code, _, err) = bohr(&["reduce", "no-such-bundle.bohr", "-o", &out.to_string_lossy()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-1002"), "{err}");
}

/// A finding reduces to a bundle the tool still replays.
///
/// The guest fails on every seed, so the test always has a finding to reduce.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_finding_reduces_to_a_bundle_that_still_replays() {
    const GUEST: &str = "testdata/guests/build/lease_clock";
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join(GUEST).is_file() {
        return;
    }
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
            .current_dir(&root)
            .args(args)
            .output()
            .expect("the binary under test must be runnable");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let dir = scratch("finding");
    let found = dir.join("found.bohr").to_string_lossy().into_owned();
    let (code, _, err) = run(&[
        "run",
        "--seed",
        "1",
        "--config",
        "testdata/bugs/lease_clock/scenario.toml",
        "--save",
        &found,
        "--",
        GUEST,
    ]);
    assert_eq!(code, 1, "the stepped clock must be found: {err}");

    let out = dir.join("smaller.bohr").to_string_lossy().into_owned();
    let (code, said, err) = run(&["reduce", &found, "-o", &out, "--budget", "40"]);
    assert_eq!(code, 0, "{err}");
    assert!(said.contains("events:"), "the result was not reported: {said}{err}");

    let (code, _, err) = run(&["replay", "--verify-only", &out]);
    assert_eq!(code, 0, "the reduced bundle no longer replays: {err}");
}

/// A bundle judged by an oracle this build does not wire is refused, naming both.
///
/// Without the oracle the bundle would replay clean and report `BOHR-5003: nothing to reduce`.
#[test]
fn a_bundle_judged_by_another_oracle_is_refused_before_replaying() {
    use bohr_trace::{Bundle, SectionId};

    let bytes = std::fs::read(stored("two-tasks-seed0.bohr")).expect("the golden bundle is there");
    let bundle = Bundle::parse(&bytes).expect("it parses");
    let manifest = String::from_utf8(
        bundle.section(SectionId::Manifest).expect("every bundle has one").to_vec(),
    )
    .expect("the manifest is text");

    // Only the oracle field changes; the rest of the bundle is the real run's.
    let foreign =
        manifest.replace(r#""name": "properties+crash""#, r#""name": "properties+boundary""#);
    assert_ne!(
        foreign, manifest,
        "the manifest must name the oracle for this test to mean anything"
    );

    let dir = scratch("foreign");
    let path = dir.join("foreign.bohr");
    let mut writer = bundle.to_writer();
    writer.section(SectionId::Manifest, foreign.into_bytes());
    std::fs::write(&path, writer.finish([0; 16])).expect("the rewritten bundle is written");

    let out = dir.join("smaller.bohr");
    let (code, _, err) = bohr(&["reduce", &path.to_string_lossy(), "-o", &out.to_string_lossy()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-4006"), "the refusal must name its code: {err}");
    assert!(err.contains("properties+boundary"), "and both sides of the mismatch: {err}");
}
