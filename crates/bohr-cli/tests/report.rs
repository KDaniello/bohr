//! `bohr report`: one file out, two only where the run was too long to draw whole.

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

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bohr-report-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory of this test's own");
    dir
}

fn stored(name: &str) -> String {
    format!("{}/../../testdata/bundles/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn a_stored_run_becomes_a_page_that_opens_on_its_own() {
    let out = scratch("page").join("run.html");
    let (code, said, err) =
        bohr(&["report", &stored("two-tasks-seed0.bohr"), "-o", &out.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    assert!(said.contains("run.html"), "the file written was not named: {said}");

    let page = std::fs::read_to_string(&out).expect("the page was written");
    assert!(page.starts_with("<!doctype html>"), "not a document");
    assert!(
        page.contains("<style>") && page.contains("<script>"),
        "the page is not self-contained"
    );
    assert!(
        !page.contains("http://") && !page.contains("https://"),
        "the page reaches for something outside itself"
    );
    assert!(page.contains("This run found"), "no verdict: the page opens with machinery");
}

/// A short run is one file; the sidecar appears only for runs too long to draw.
#[test]
fn a_short_run_is_one_file() {
    let dir = scratch("single");
    let out = dir.join("run.html");
    let (code, _, err) =
        bohr(&["report", &stored("clock-and-random-seed0.bohr"), "-o", &out.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");

    let written: Vec<_> = std::fs::read_dir(&dir)
        .expect("the directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(written, vec!["run.html"], "got {written:?}");
}

#[test]
fn a_bundle_that_is_not_there_is_a_usage_error() {
    let out = scratch("absent").join("run.html");
    let (code, _, err) = bohr(&["report", "no-such-bundle.bohr", "-o", &out.to_string_lossy()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-1002"), "{err}");
}

/// The page is valid text in the encoding it declares.
#[test]
fn the_page_is_the_encoding_it_says_it_is() {
    let out = scratch("encoding").join("run.html");
    let (code, _, err) =
        bohr(&["report", &stored("large-random-seed0.bohr"), "-o", &out.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");

    let bytes = std::fs::read(&out).expect("the page");
    let page = String::from_utf8(bytes).expect("the page is valid utf-8");
    assert!(page.contains("charset=\"utf-8\""), "the page does not declare its encoding");
}

/// A redacted bundle still replays (§4.6).
#[test]
fn a_redacted_bundle_is_still_one_the_tool_accepts() {
    let dir = scratch("redact");
    let original = dir.join("run.bohr");
    std::fs::copy(stored("two-tasks-seed0.bohr"), &original).expect("a copy to work on");

    let out = dir.join("run.html");
    let (code, said, err) =
        bohr(&["report", &original.to_string_lossy(), "-o", &out.to_string_lossy(), "--redact"]);
    assert_eq!(code, 0, "{err}");
    assert!(said.contains("redacted.bohr"), "the redacted bundle was not named: {said}");

    let redacted = dir.join("run.redacted.bohr");
    assert!(redacted.is_file(), "no redacted bundle was written");
    let (code, _, err) = bohr(&["replay", "--verify-only", &redacted.to_string_lossy()]);
    assert_eq!(code, 0, "the redacted bundle no longer replays: {err}");
}

/// A redacted bundle says it is redacted.
#[test]
fn a_redacted_bundle_says_that_it_is_one() {
    let dir = scratch("redact-note");
    let original = dir.join("run.bohr");
    std::fs::copy(stored("clock-and-random-seed0.bohr"), &original).expect("a copy");

    let (code, _, err) = bohr(&[
        "report",
        &original.to_string_lossy(),
        "-o",
        &dir.join("run.html").to_string_lossy(),
        "--redact",
    ]);
    assert_eq!(code, 0, "{err}");

    let bytes = std::fs::read(dir.join("run.redacted.bohr")).expect("the redacted bundle");
    let bundle = bohr_trace::Bundle::parse(&bytes).expect("still a bundle");
    let notes = bundle
        .section(bohr_trace::SectionId::Notes)
        .map(|note| String::from_utf8_lossy(note).into_owned())
        .unwrap_or_default();
    assert!(notes.contains("redacted:"), "the bundle does not say what was taken out: {notes}");
}

/// Notes survive a write and read back, and a second note does not replace the first.
#[test]
fn a_note_is_kept_on_the_bundle_and_added_to() {
    let dir = scratch("note");
    let bundle = dir.join("run.bohr");
    std::fs::copy(stored("two-tasks-seed0.bohr"), &bundle).expect("a copy to write on");

    let (code, said, err) =
        bohr(&["note", &bundle.to_string_lossy(), "reported upstream as issue 412"]);
    assert_eq!(code, 0, "{err}");
    assert!(said.contains("issue 412"), "{said}");

    let (code, said, err) = bohr(&["note", &bundle.to_string_lossy(), "fixed in 1.4.2"]);
    assert_eq!(code, 0, "{err}");
    assert!(said.contains("issue 412"), "the first note was replaced: {said}");
    assert!(said.contains("fixed in 1.4.2"), "{said}");

    let (code, _, err) = bohr(&["replay", "--verify-only", &bundle.to_string_lossy()]);
    assert_eq!(code, 0, "a bundle with a note on it no longer replays: {err}");
}

/// The same note written twice gives the same bytes; it holds no timestamp.
#[test]
fn a_note_carries_nothing_about_when_it_was_made() {
    let dir = scratch("note-twice");
    let (first, second) = (dir.join("a.bohr"), dir.join("b.bohr"));
    std::fs::copy(stored("two-tasks-seed0.bohr"), &first).expect("a copy");
    std::fs::copy(stored("two-tasks-seed0.bohr"), &second).expect("another copy");

    for path in [&first, &second] {
        let (code, _, err) = bohr(&["note", &path.to_string_lossy(), "the same words"]);
        assert_eq!(code, 0, "{err}");
    }
    assert_eq!(
        std::fs::read(&first).expect("a"),
        std::fs::read(&second).expect("b"),
        "two identical notes produced different bundles"
    );
}
