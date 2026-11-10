//! What `bohr doctor` must say, and must never do.
//!
//! Runs on any host. The verdict varies; these check that it is reached, named and never acted
//! on, including on hosts that cannot run guests.

use std::process::Command;

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary must be runnable");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn a_diagnosis_reaches_a_verdict_and_names_what_it_checked() {
    let (code, out) = bohr(&["doctor"]);
    // 0 for a usable host, 4 for an unusable one.
    assert!(code == 0 || code == 4, "doctor exited {code}:\n{out}");
    assert!(
        out.contains("pass") || out.contains("skip") || out.contains("FAIL"),
        "no check was reported:\n{out}"
    );
    assert!(
        out.contains("this host can") || out.contains("cannot run guests"),
        "no verdict was reached:\n{out}"
    );
}

/// A failure says what to type next.
#[test]
fn every_failure_carries_something_to_type() {
    let (_, out) = bohr(&["doctor"]);
    for (i, line) in out.lines().enumerate() {
        if line.contains("FAIL") {
            let next = out.lines().nth(i + 1).unwrap_or_default();
            assert!(
                !next.trim().is_empty() && next.starts_with(' '),
                "a failure with no remedy under it:\n  {line}\n  {next}"
            );
        }
    }
}

/// The support table prints.
#[test]
fn the_syscall_table_can_be_printed() {
    let (code, out) = bohr(&["doctor", "--syscalls"]);
    assert_eq!(code, 0, "{out}");
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        assert!(out.contains("clock_gettime"), "the table is missing what it must have:\n{out}");
        assert!(out.contains("futex"), "{out}");
    } else {
        assert!(out.contains("no syscall backend"), "{out}");
    }
}

/// A target that cannot be read is a usage error.
#[test]
fn a_missing_target_is_refused_rather_than_judged() {
    let (code, out) = bohr(&["doctor", "--target", "no/such/program"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("BOHR-1002"), "the refusal must name its code:\n{out}");
}

/// Inspecting a program says what it will need, without running it.
#[test]
fn a_target_is_inspected_where_one_can_be() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/guests/build/clock_and_random");
    if !path.is_file() || !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return;
    }
    let (code, out) = bohr(&["doctor", "--target", &path.to_string_lossy()]);
    assert!(code == 0 || code == 4, "{out}");
    assert!(out.contains("target loader"), "the loader was not reported:\n{out}");
    assert!(out.contains("hardware randomness"), "the scan was not reported:\n{out}");
    assert!(out.contains("io_uring"), "{out}");
}

/// The instruction is reported where it is, and not where its bytes appear inside another one.
///
/// One guest has a single `RDRAND`; the other has none but contains its byte pattern inside
/// ordinary instructions' displacements. A real `libstdc++` has five sites.
#[test]
fn hardware_randomness_is_reported_where_it_is_and_only_there() {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return;
    }
    let guest = |name: &str| {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/guests/build")
            .join(name)
    };

    let carries = guest("asks_the_processor_for_entropy");
    let plain = guest("two_threads");
    if !carries.is_file() || !plain.is_file() {
        return;
    }

    let (_, out) = bohr(&["doctor", "--target", &carries.to_string_lossy()]);
    assert!(out.contains("1 site(s)"), "the one instruction there must be named:\n{out}");

    let (_, out) = bohr(&["doctor", "--target", &plain.to_string_lossy()]);
    assert!(
        out.contains("no RDRAND or RDSEED found"),
        "and a guest without one must not be accused:\n{out}"
    );
}
