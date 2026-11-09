//! Every subject in this repository, held to the checks it declares.
//!
//! The test does not list subjects: it reads them, as `bohr check --all` does, so a pack or a
//! seeded defect added as files is checked here without a line of Rust. What it asserts is that
//! the declarations parse and that every defect with a scenario has one; the quick checks run in
//! `make test`, and the full ones — sixty-seed sweeps, a thousand repeats — in `make test-hunting`.
//!
//! Packs are declared here and checked in the VM gate, not in this file: their binaries are built
//! by their own pack, take minutes to build, and are not in a checkout.

use std::{path::PathBuf, process::Command};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .current_dir(root())
        .args(args)
        .output()
        .expect("the binary must be runnable");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

/// Every declaration parses, and none of them is empty.
#[test]
fn every_subject_declares_its_checks() {
    let (code, text) = bohr(&["check", "--all", "--plan"]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("no checks declared"), "a subject with nothing to hold it to:\n{text}");
    for pack in ["geth", "cometbft", "reth", "redb"] {
        assert!(
            text.contains(&format!("{pack} · targets/{pack}")),
            "{pack} is not a subject:\n{text}"
        );
    }
}

/// A seeded defect with a scenario is a subject: a scenario nobody checks is a gate nobody runs.
#[test]
fn every_seeded_defect_with_a_scenario_has_a_bug_toml() {
    let bugs = root().join("testdata/bugs");
    for entry in std::fs::read_dir(&bugs).expect("testdata/bugs is in the checkout").flatten() {
        let dir = entry.path();
        let has_scenario = std::fs::read_dir(&dir).into_iter().flatten().flatten().any(|f| {
            let name = f.file_name().to_string_lossy().into_owned();
            name.starts_with("scenario") && name.ends_with(".toml")
        });
        if has_scenario {
            assert!(
                dir.join("bug.toml").is_file(),
                "{} has a scenario and no bug.toml",
                dir.display()
            );
        }
    }
}

/// The subjects whose programs are built here, by their directory and the program they run.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn built_defects() -> Vec<&'static str> {
    [
        ("alloc_unchecked", "testdata/guests/build/alloc_unchecked"),
        ("kv_ack_quiet", "testdata/guests/build/kv_ack_quiet"),
        ("kv_ack_quiet_correct", "testdata/guests/build/kv_ack_quiet_correct"),
        ("lease_clock", "testdata/guests/build/lease_clock"),
        ("chain-toy", "examples/chain-toy/build/chain-toy-node"),
    ]
    .into_iter()
    .filter(|(_, program)| root().join(program).is_file())
    .map(|(name, _)| name)
    .collect()
}

/// The quick checks — that each program works, repeats and replays — hold on every built defect.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn quick_checks_hold_on_every_built_defect() {
    let out = std::env::temp_dir().join(format!("bohr-subjects-quick-{}", std::process::id()));
    for name in built_defects() {
        let (code, text) = bohr(&["check", name, "--quick", "-o", &out.to_string_lossy()]);
        assert_eq!(code, 0, "{name}:\n{text}");
    }
    let _ = std::fs::remove_dir_all(&out);
}

/// Every check every built defect declares holds: the seeded defects are found at their bounds, and
/// their correct twins are not.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
#[ignore = "sweeps of sixty seeds and a thousand repeats; run by `make test-hunting`"]
fn every_check_holds_on_every_built_defect() {
    let out = std::env::temp_dir().join(format!("bohr-subjects-full-{}", std::process::id()));
    let mut failed = Vec::new();
    for name in built_defects() {
        let (code, text) = bohr(&["check", name, "-o", &out.to_string_lossy()]);
        eprintln!("{text}");
        if code != 0 {
            failed.push(name);
        }
    }
    let _ = std::fs::remove_dir_all(&out);
    assert!(failed.is_empty(), "checks that did not hold: {failed:?}");
}
