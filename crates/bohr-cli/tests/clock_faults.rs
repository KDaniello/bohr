//! Clock faults against a real program: a wall clock set back is reached, and nothing else is.
//!
//! Linux only, and skipped where the guest has not been built, like the other tests of real guests.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{path::PathBuf, process::Command};

/// The guest, relative to the repository root the commands run in.
const GUEST: &str = "testdata/guests/build/lease_clock";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .current_dir(root())
        .args(args)
        .output()
        .expect("the binary must be runnable");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn built() -> bool {
    root().join(GUEST).is_file()
}

/// A step back during the lease is the finding, on every seed: the moment is written down.
#[test]
fn a_wall_clock_set_back_outlives_a_lease() {
    if !built() {
        return;
    }
    for seed in ["1", "2", "3"] {
        let (code, report) = bohr(&[
            "run",
            "--seed",
            seed,
            "--config",
            "testdata/bugs/lease_clock/scenario.toml",
            "--",
            GUEST,
        ]);
        assert_eq!(code, 1, "the stepped clock must be found on seed {seed}: {report}");
    }
}

/// Without the step the lease runs out on time: skew and drift move both clocks together.
#[test]
fn a_steady_clock_keeps_the_lease_honest() {
    if !built() {
        return;
    }
    for seed in ["1", "2", "3"] {
        let (code, report) = bohr(&[
            "run",
            "--seed",
            seed,
            "--config",
            "testdata/bugs/lease_clock/scenario-steady.toml",
            "--",
            GUEST,
        ]);
        assert_eq!(code, 0, "no clock fault, no finding on seed {seed}: {report}");
    }
}

/// An absolute wall-clock deadline set after a step is reached a second later, not thirty-one.
#[test]
fn a_wall_deadline_is_measured_on_the_wall_clock() {
    const DEADLINE: &str = "testdata/guests/build/wall_deadline";
    if !root().join(DEADLINE).is_file() {
        return;
    }
    let (code, report) = bohr(&[
        "run",
        "--seed",
        "1",
        "--config",
        "testdata/guests/wall_deadline.toml",
        "--",
        DEADLINE,
    ]);
    assert_eq!(code, 0, "the deadline must be one second of monotonic time away: {report}");
}

/// A run with clock faults drawn repeats to one digest.
#[test]
fn drawn_clock_faults_repeat() {
    if !built() {
        return;
    }
    let (code, report) = bohr(&[
        "run",
        "--seed",
        "7",
        "--repeat",
        "50",
        "--config",
        "testdata/bugs/lease_clock/scenario-drawn.toml",
        "--",
        GUEST,
    ]);
    assert!(code == 0 || code == 1, "{report}");
    assert!(!report.contains("DIVERGED"), "{report}");
}
