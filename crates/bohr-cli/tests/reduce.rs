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

/// A bundle that records nothing going wrong has nothing to reduce, and saying so is more useful
/// than spending two thousand replays discovering it.
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

/// The one that matters: a finding reduces, and what comes out is a bundle the tool still accepts.
/// A reduction that produced something unreplayable would have made the finding harder to hand on
/// rather than easier.
#[test]
fn a_finding_reduces_to_a_bundle_that_still_replays() {
    let dir = scratch("finding");
    let findings = dir.join("findings");
    let (code, _, err) = bohr(&[
        "explore",
        "--guest",
        "many-flips",
        "--runs",
        "64",
        "--master-seed",
        "11",
        "--findings",
        &findings.to_string_lossy(),
    ]);
    assert!(matches!(code, 0 | 1), "the campaign did not complete: {err}");

    let Some(found) = std::fs::read_dir(&findings)
        .ok()
        .and_then(|entries| entries.flatten().map(|e| e.path()).find(|p| p.is_file()))
    else {
        // The demo guest is not always made to fail by a short campaign, and inventing a finding
        // here would test the fixture rather than the command.
        eprintln!("the campaign found nothing to reduce; nothing to check");
        return;
    };

    let out = dir.join("smaller.bohr");
    let (code, said, err) =
        bohr(&["reduce", &found.to_string_lossy(), "-o", &out.to_string_lossy(), "--budget", "40"]);
    assert_eq!(code, 0, "{err}");
    assert!(said.contains("events:"), "the result was not reported: {said}{err}");
    assert!(out.is_file(), "no bundle was written");

    let (code, _, err) = bohr(&["replay", "--verify-only", &out.to_string_lossy()]);
    assert_eq!(code, 0, "the reduced bundle no longer replays: {err}");
}

/// A bundle judged by an oracle this build does not wire is refused, naming both sides.
///
/// Before the check existed, such a bundle replayed clean — the oracle that found the violation was
/// not there to find it again — and the reduction reported `BOHR-5003: nothing to reduce`. That
/// message is true about what happened and points a reader at the bundle, which is the wrong place.
#[test]
fn a_bundle_judged_by_another_oracle_is_refused_before_replaying() {
    use bohr_trace::{Bundle, SectionId};

    let bytes = std::fs::read(stored("two-tasks-seed0.bohr")).expect("the golden bundle is there");
    let bundle = Bundle::parse(&bytes).expect("it parses");
    let manifest = String::from_utf8(
        bundle.section(SectionId::Manifest).expect("every bundle has one").to_vec(),
    )
    .expect("the manifest is text");

    // The one field under test, changed where it is written rather than by rebuilding a run: what
    // is being checked is the refusal, and a hand-built bundle would be checking the builder.
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
