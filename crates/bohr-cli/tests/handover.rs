//! `bohr replay --gdb` and `--break-at`: everything that is refused, and why.
//!
//! The session needs a terminal, a debugger and Linux, so it is untested. These check that every
//! refusal happens before the replay runs.

use std::process::Command;

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary under test must be runnable");
    let mut said = String::from_utf8_lossy(&out.stdout).into_owned();
    said.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), said)
}

fn stored(name: &str) -> String {
    format!("{}/../../testdata/bundles/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// `--gdb` and `--break-at` together are refused.
#[test]
fn naming_both_flags_is_refused() {
    let (code, said) =
        bohr(&["replay", "--gdb", "--break-at", "10", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains("BOHR-1001"), "{said}");
    assert!(said.contains("--break-at 0"), "the refusal must say what they mean: {said}");
}

/// A hand-over with `--verify-only` is refused; it never reaches a digest.
#[test]
fn a_hand_over_with_verify_only_is_refused() {
    let (code, said) = bohr(&["replay", "--gdb", "--verify-only", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains("BOHR-1001"), "{said}");
}

/// A hand-over takes exactly one bundle.
#[test]
fn a_hand_over_of_several_bundles_is_refused() {
    let (code, said) = bohr(&[
        "replay",
        "--gdb",
        &stored("two-tasks-seed0.bohr"),
        &stored("clock-and-random-seed0.bohr"),
    ]);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains("one bundle"), "{said}");
}

/// An event past the recorded count is refused before running.
#[test]
fn a_break_past_the_end_of_the_recorded_run_is_refused() {
    let (code, said) = bohr(&["replay", "--break-at", "999999", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains("BOHR-1002"), "{said}");
    assert!(said.contains("cannot be reached"), "{said}");
}

/// A built-in guest has no process to attach to and is refused.
#[test]
fn a_built_in_guest_has_no_process_to_hand_over() {
    let (code, said) = bohr(&["replay", "--gdb", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 3, "{said}");
    assert!(said.contains("BOHR-3006"), "{said}");
    assert!(said.contains("built into the tool"), "{said}");
}

/// An ordinary replay is unaffected.
#[test]
fn a_replay_without_the_flags_still_verifies() {
    let (code, said) = bohr(&["replay", "--verify-only", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("ok blake3:"), "{said}");
}
