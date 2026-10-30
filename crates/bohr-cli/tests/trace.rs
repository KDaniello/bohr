//! `bohr trace`: what a stored run recorded, without running anything.

use std::process::Command;

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

fn stored(name: &str) -> String {
    format!("{}/../../testdata/bundles/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn a_stored_run_prints_one_line_per_event() {
    let bundle = stored("two-tasks-seed0.bohr");
    let (code, out, err) = bohr(&["trace", &bundle]);
    assert_eq!(code, 0, "{err}");

    let lines = out.lines().count();
    assert!(lines > 4, "a run of several tasks printed {lines} lines");
    assert!(out.contains("syscall"), "no syscall was named: {out}");
}

/// The json form is what a report points at for the events it did not show, so every line of it
/// has to be readable on its own.
#[test]
fn the_json_form_is_one_object_per_line() {
    let bundle = stored("clock-and-random-seed0.bohr");
    let (code, out, err) = bohr(&["trace", &bundle, "--format", "json"]);
    assert_eq!(code, 0, "{err}");

    for line in out.lines() {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert!(value.get("id").is_some(), "{line} has no id");
        assert!(value.get("kind").is_some(), "{line} has no kind");
    }
}

/// A format nobody implements is refused by name rather than silently answered with another one.
#[test]
fn an_unknown_format_is_refused_and_says_what_there_is() {
    let (code, _, err) = bohr(&["trace", &stored("two-tasks-seed0.bohr"), "--format", "yaml"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-1001"), "the refusal must name its code: {err}");
    assert!(err.contains("text") && err.contains("json"), "and say what there is: {err}");
}

#[test]
fn a_bundle_that_is_not_there_is_a_usage_error() {
    let (code, _, err) = bohr(&["trace", "no-such-bundle.bohr"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-1002"), "{err}");
}
