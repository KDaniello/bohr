//! A file moved through an `io_uring` ring, against a real program.
//!
//! Linux only; skips when the guest is not built.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{path::PathBuf, process::Command};

/// The guest and the scenario that shows it a ring, relative to the repository root.
const GUEST: &str = "testdata/guests/build/ring_files";
const RING: &str = "testdata/guests/ring_files.toml";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .current_dir(root())
        .args(args)
        .output()
        .expect("the binary must be runnable");
    let mut report = String::from_utf8_lossy(&out.stdout).into_owned();
    report.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), report)
}

fn built() -> bool {
    root().join(GUEST).is_file()
}

/// Open, write, sync, read, stat, close and unlink, all through submissions, on every seed.
#[test]
fn a_file_moves_through_a_ring() {
    if !built() {
        return;
    }
    for seed in ["1", "2", "3"] {
        let (code, report) = bohr(&["run", "--seed", seed, "--config", RING, "--", GUEST]);
        assert_eq!(code, 0, "every completion must be right on seed {seed}: {report}");
    }
}

/// The default is the kernel without the ring: setup is declined and the guest says so.
#[test]
fn without_the_key_there_is_no_ring() {
    if !built() {
        return;
    }
    let (code, report) = bohr(&["run", "--seed", "1", "--", GUEST]);
    assert_eq!(code, 1, "the guest exits 10 when setup is declined: {report}");
    assert!(report.contains("exit status 10"), "{report}");
    assert!(report.contains("declined"), "{report}");
}

/// A run through the ring repeats to one digest.
#[test]
fn a_ring_repeats() {
    if !built() {
        return;
    }
    let (code, report) =
        bohr(&["run", "--seed", "7", "--repeat", "50", "--config", RING, "--", GUEST]);
    assert_eq!(code, 0, "{report}");
    assert!(!report.contains("DIVERGED"), "{report}");
}

/// A sync through the ring goes through the disk model.
///
/// With every sync drawn to fail, the write lands and the linked sync completes `EIO`; exit 19 is
/// that step.
#[test]
fn a_sync_through_the_ring_meets_the_disk_model() {
    if !built() {
        return;
    }
    let scenario = std::env::temp_dir().join(format!("bohr-ring-eio-{}.toml", std::process::id()));
    std::fs::write(&scenario, "[linux]\nio_uring = \"ring\"\n[disk]\neio_probability = \"1/1\"\n")
        .expect("the scenario is written");
    let path = scenario.to_string_lossy().into_owned();
    let (code, report) = bohr(&["run", "--seed", "1", "--config", &path, "--", GUEST]);
    let _ = std::fs::remove_file(&scenario);
    assert_eq!(code, 1, "{report}");
    assert!(report.contains("exit status 19"), "the sync step, not the write: {report}");
}
