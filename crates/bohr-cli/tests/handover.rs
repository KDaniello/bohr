//! `bohr replay --gdb` and `--break-at`: everything that is refused, and why.
//!
//! The session itself is not tested and cannot be: it needs a terminal, a debugger and a Linux
//! host. What is tested is that nothing gets as far as a replay it cannot use — which is where all
//! the cost is, because a refusal after replaying has already spent the run it exists to save.

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

/// The two flags say the same thing, so naming both says two different things.
#[test]
fn naming_both_flags_is_refused() {
    let (code, said) =
        bohr(&["replay", "--gdb", "--break-at", "10", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains("BOHR-1001"), "{said}");
    assert!(said.contains("--break-at 0"), "the refusal must say what they mean: {said}");
}

/// A hand-over never reaches a digest, so asking it to check one is asking for two things at once.
#[test]
fn a_hand_over_with_verify_only_is_refused() {
    let (code, said) = bohr(&["replay", "--gdb", "--verify-only", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains("BOHR-1001"), "{said}");
}

/// A hand-over is one frozen world. Several bundles would be several worlds, and only one of them
/// could be handed anywhere.
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

/// An event the run never recorded cannot be stopped at, and the bundle already says how many it
/// recorded — so this is answerable without running anything.
#[test]
fn a_break_past_the_end_of_the_recorded_run_is_refused() {
    let (code, said) = bohr(&["replay", "--break-at", "999999", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 2, "{said}");
    assert!(said.contains("BOHR-1002"), "{said}");
    assert!(said.contains("cannot be reached"), "{said}");
}

/// A guest built into the tool is a data structure in this process. There is nothing to attach to,
/// and saying so is more useful than a debugger session that cannot exist.
#[test]
fn a_built_in_guest_has_no_process_to_hand_over() {
    let (code, said) = bohr(&["replay", "--gdb", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 3, "{said}");
    assert!(said.contains("BOHR-3006"), "{said}");
    assert!(said.contains("built into the tool"), "{said}");
}

/// An ordinary replay is unaffected by any of this.
#[test]
fn a_replay_without_the_flags_still_verifies() {
    let (code, said) = bohr(&["replay", "--verify-only", &stored("two-tasks-seed0.bohr")]);
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("ok blake3:"), "{said}");
}
