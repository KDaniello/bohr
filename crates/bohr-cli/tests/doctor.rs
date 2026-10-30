//! What `bohr doctor` must say, and must never do.
//!
//! Runs on any host: what varies is the verdict, and what these check is that a verdict is
//! reached, named, and never acted on. A host that fails is as valid an outcome as one that
//! passes — the gate is that the answer is correct on every runner, including the ones that
//! cannot run guests at all.

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
    // Zero where the host is usable, four where it is not; nothing else is a diagnosis.
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

/// A failure has to say what to type. A diagnosis that names a problem and not a next step is
/// where somebody gives up.
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

/// The support table is what a target is compared against, so it must be printable.
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

/// A target that cannot be read is a usage error, not a verdict about the host.
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

/// The instruction is reported where it is there, and not where it only looks like it.
///
/// Two guests, because either half alone is a scan that could be right by accident. One carries a
/// single `RDRAND`; the other carries none and carries, like every large object, byte pairs that
/// spell one inside the displacements of ordinary instructions.
///
/// The strict half is the one that was wrong. A rule that dropped every candidate it had not
/// placed on an instruction boundary reported `libstdc++` clean — five real sites, the path
/// `std::random_device` takes — and a hole reported as solid ground is worse than a warning that
/// wastes an afternoon.
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
