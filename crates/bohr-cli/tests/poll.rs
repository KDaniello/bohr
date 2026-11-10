//! `poll` over a socket in a cluster, against a real program.
//!
//! Linux only; skips when the guest is not built.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{path::PathBuf, process::Command};

/// The guest and its two-node scenario, relative to the repository root.
const GUEST: &str = "testdata/guests/build/poll_socket";
const SCENARIO: &str = "testdata/guests/poll_socket.toml";

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

/// A poll that finds nothing returns at once; one that waits is woken by the datagram, on every
/// seed, and repeats to one digest.
#[test]
fn a_poll_is_woken_by_the_datagram_it_waits_for() {
    if !root().join(GUEST).is_file() {
        return;
    }
    for seed in ["1", "2", "3"] {
        let (code, report) = bohr(&["run", "--seed", seed, "--config", SCENARIO, "--", GUEST]);
        assert_eq!(code, 0, "node 0 must see its socket readable on seed {seed}: {report}");
    }
    let (code, report) =
        bohr(&["run", "--seed", "9", "--repeat", "50", "--config", SCENARIO, "--", GUEST]);
    assert_eq!(code, 0, "{report}");
    assert!(!report.contains("DIVERGED"), "{report}");
}
