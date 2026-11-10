//! `bohr explain` and the trace filters, read from stored bundles: no backend is needed, so these
//! run on every host.

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

const BUNDLES: [&str; 3] =
    ["clock-and-random-seed0.bohr", "large-random-seed0.bohr", "two-tasks-seed0.bohr"];

/// One bundle tells one story every time, and the JSON carries every field the text uses.
#[test]
fn a_stored_run_tells_the_same_story_every_time() {
    for name in BUNDLES {
        let path = stored(name);
        let (code, first, err) = bohr(&["explain", &path]);
        assert_eq!(code, 0, "{name}: {err}");
        assert!(first.starts_with("What was found"), "{name}:\n{first}");
        assert!(first.contains("What the run was"), "{name}:\n{first}");
        assert!(!first.contains("(\""), "{name} reads like debug output:\n{first}");
        let (_, again, _) = bohr(&["explain", &path]);
        assert_eq!(first, again, "{name} told two stories");

        let (code, json, err) = bohr(&["explain", &path, "--json"]);
        assert_eq!(code, 0, "{name}: {err}");
        let value: serde_json::Value = serde_json::from_str(&json).expect("one JSON object");
        let headline = value["headline"].as_str().expect("a headline");
        assert!(first.contains(headline), "{name}: the text and the JSON say different things");
        for field in ["found", "code", "run", "steps", "next"] {
            assert!(value.get(field).is_some(), "{name}: no {field}");
        }
    }
}

/// Selecting by kind loses nothing: a kind and every other kind together are the whole log.
#[test]
fn a_kind_and_its_complement_are_the_whole_log() {
    let kinds = [
        "syscall",
        "schedule",
        "task",
        "packet",
        "disk",
        "fault",
        "node",
        "wait",
        "clock",
        "assertion",
        "record",
        "diagnostic",
        "domain",
    ];
    for name in BUNDLES {
        let path = stored(name);
        let (_, all, _) = bohr(&["trace", &path]);
        let total = all.lines().count();
        for kind in kinds {
            let others: Vec<&str> = kinds.iter().copied().filter(|k| *k != kind).collect();
            let (_, one, _) = bohr(&["trace", &path, "--kind", kind]);
            let (_, rest, _) = bohr(&["trace", &path, "--kind", &others.join(",")]);
            assert_eq!(
                one.lines().count() + rest.lines().count(),
                total,
                "{name}: --kind {kind} and its complement lost or doubled events"
            );
        }
    }
}

/// A time is read exactly as written, and an unknown kind is refused.
#[test]
fn a_selection_is_read_exactly_or_refused() {
    let path = stored("two-tasks-seed0.bohr");
    let (code, _, err) = bohr(&["trace", &path, "--kind", "syscalls"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("no kind of event named"), "{err}");
    let (code, _, err) = bohr(&["trace", &path, "--from", "1.5h"]);
    assert_eq!(code, 2, "{err}");
    let (code, out, _) = bohr(&["trace", &path, "--from", "0s", "--to", "0.000001s"]);
    assert_eq!(code, 0);
    assert!(out.lines().count() < bohr(&["trace", &path]).1.lines().count());
}
