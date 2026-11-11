//! The laws of durability against the defects seeded for them, under real interception.
//!
//! Linux only; each test skips itself when its guest from `testdata/guests` is not built.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{path::PathBuf, process::Command};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Runs `seed` of a seeded defect's scenario from the repository root; its code and its report.
fn run(defect: &str, scenario: &str, seed: u64) -> Option<(i32, String)> {
    let guest = format!("testdata/guests/build/{defect}");
    if !root().join(&guest).is_file() {
        return None;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .current_dir(root())
        .args(["run", "--seed", &seed.to_string(), "--config"])
        .arg(format!("testdata/bugs/{defect}/{scenario}"))
        .args(["--", &guest])
        .output()
        .expect("the binary must be runnable");
    Some((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned()))
}

/// Each law catches the defect seeded for it, and its finding names the changed file (G9.2.1,
/// G9.2.4).
#[test]
fn each_law_catches_its_defect_and_names_the_file() {
    for (defect, code) in [
        ("rename_without_dirsync", "BOHR-2051"),
        ("delete_unsynced", "BOHR-2057"),
        ("batch_in_two_writes", "BOHR-2058"),
        ("restart_refuses_new_store", "BOHR-2055"),
    ] {
        let Some((exit, report)) = run(defect, "scenario.toml", 1) else { continue };
        assert_eq!(exit, 1, "{defect}: {report}");
        assert!(report.contains(code), "{defect} was not caught by {code}: {report}");
        if code != "BOHR-2055" {
            assert!(report.contains(&format!("{defect}.c:")), "{defect} is not placed: {report}");
        }
    }
}

/// The boundary's judgement and the law's find the same seeds of `kv_ack_quiet` (G9.2.5).
#[test]
fn the_boundary_and_the_law_find_the_same_seeds() {
    for seed in 1..=8 {
        let Some((by_boundary, said)) = run("kv_ack_quiet", "scenario.toml", seed) else { return };
        let Some((by_law, told)) = run("kv_ack_quiet", "scenario-law.toml", seed) else { return };
        assert_eq!(by_boundary, by_law, "seed {seed}:\n{said}\n{told}");
    }
}
