//! What a guest learns of the host it runs on: nothing that differs between two hosts.
//!
//! Linux only; each test skips itself when its guest from `testdata/guests` is not built.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

fn guest(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("testdata/guests/build")
        .join(name);
    path.is_file().then_some(path)
}

/// A fresh directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bohr-host-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Runs `command` and returns its exit code, standard output and standard error.
fn output(command: &mut Command) -> (i32, String, String) {
    let out = command.output().expect("the binary must be runnable");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The run's digest, as its report names it.
fn digest(report: &str) -> &str {
    let at = report.find("blake3:").unwrap_or_else(|| panic!("no digest in: {report}"));
    &report[at..(at + 71).min(report.len())]
}

/// One guest, copied into directories of two lengths, run on two cores and under an unlimited
/// stack, prints one set of facts and reaches one digest.
///
/// Covers the executed path and `AT_EXECFN` (D1), the auxiliary vector (D4), the stack limit (D6),
/// `MADV_FREE` (D7), `rseq` and `sched_getcpu` (D2), and the signal state the supervisor would
/// otherwise hand down (S10).
#[test]
fn the_host_does_not_reach_the_guest() {
    let Some(facts) = guest("machine_facts") else {
        return;
    };
    let root = scratch("facts");
    let near = root.join("a");
    let far = root.join("a-directory-name-much-longer-than-the-first-one-by-far");
    let mut runs = Vec::new();
    for (dir, core, unlimited) in
        [(&near, "0", false), (&far, "0", false), (&near, "1", false), (&near, "0", true)]
    {
        std::fs::create_dir_all(dir).expect("dir");
        std::fs::copy(&facts, dir.join("machine_facts")).expect("copy");
        let mut command = if unlimited {
            let mut shell = Command::new("sh");
            shell.args([
                "-c",
                "ulimit -s unlimited && exec \"$0\" \"$@\"",
                env!("CARGO_BIN_EXE_bohr"),
            ]);
            shell
        } else {
            Command::new(env!("CARGO_BIN_EXE_bohr"))
        };
        command
            .args(["run", "--seed", "1", "--", "./machine_facts"])
            .current_dir(dir)
            .env("BOHR_CORE", core);
        let (code, printed, report) = output(&mut command);
        assert_eq!(code, 0, "{report}");
        runs.push((printed, digest(&report).to_owned(), report));
    }
    let (printed, first, _) = &runs[0];
    for (other, digest, report) in &runs[1..] {
        assert_eq!(other, printed, "{report}");
        assert_eq!(digest, first, "{report}");
    }
    let has =
        |line: &str| assert!(printed.lines().any(|l| l == line), "no {line:?} in:\n{printed}");
    has("argv0 ./machine_facts");
    // glibc answers `AT_HWCAP` from its own constant on x86-64; the rest is the vector's.
    assert!(printed.contains("hwcap2 0 minsigstksz 8192"), "{printed}");
    has("stack limit 8388608");
    has("cpu 0");
    has("sigpipe default blocked 0");
    has("freed page 00");
    let execfn = printed.lines().find(|l| l.starts_with("execfn ")).expect("an execfn line");
    assert!(execfn.starts_with("execfn 512 /") && execfn.ends_with("/./machine_facts"), "{execfn}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A child created untraced is traced all the same, and its write reaches the world (S14).
#[test]
fn an_untraced_child_is_traced_all_the_same() {
    let Some(path) = guest("clone_untraced") else {
        return;
    };
    let (code, printed, report) = output(Command::new(env!("CARGO_BIN_EXE_bohr")).args([
        "run",
        "--seed",
        "1",
        "--repeat",
        "20",
        "--",
        &path.to_string_lossy(),
    ]));
    assert_eq!(code, 0, "{report}");
    assert!(!report.contains("DIVERGED"), "{report}");
    assert!(printed.contains("child\n") && printed.contains("parent\n"), "{printed}");
}

/// A child in namespaces of its own is refused by name (S14).
#[test]
fn a_child_in_new_namespaces_is_refused() {
    let Some(path) = guest("clone_namespace") else {
        return;
    };
    let (code, _, report) = output(Command::new(env!("CARGO_BIN_EXE_bohr")).args([
        "run",
        "--seed",
        "1",
        "--",
        &path.to_string_lossy(),
    ]));
    assert_ne!(code, 0, "{report}");
    assert!(report.contains("new namespaces"), "{report}");
}

/// A fault that ends a process with busy threads ends it the same way on every run (D3).
#[test]
fn a_fault_that_ends_a_threaded_process_repeats() {
    let Some(path) = guest("dies_with_threads") else {
        return;
    };
    let (code, _, report) = output(Command::new(env!("CARGO_BIN_EXE_bohr")).args([
        "run",
        "--seed",
        "1",
        "--repeat",
        "200",
        "--",
        &path.to_string_lossy(),
    ]));
    assert_eq!(code, 1, "a crash is a finding: {report}");
    assert!(report.contains("signal 11"), "{report}");
    assert!(!report.contains("DIVERGED"), "{report}");
}

/// The copy `--patch-cpuid` executes is placed in the user's own directory, which is refused when
/// another user could write it (S4, S8).
#[test]
fn programs_bohr_supplies_run_from_a_private_directory() {
    let Some(facts) = guest("machine_facts") else {
        return;
    };
    let tmp = scratch("private");
    let uid = std::fs::metadata("/proc/self")
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
        .expect("uid");
    let private = tmp.join(format!("bohr-{uid}"));
    let run = || {
        output(
            Command::new(env!("CARGO_BIN_EXE_bohr"))
                .args(["run", "--seed", "1", "--patch-cpuid", "--", &facts.to_string_lossy()])
                .env("TMPDIR", &tmp),
        )
    };

    std::fs::create_dir(&private).expect("dir");
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o777)).expect("mode");
    let (code, _, report) = run();
    assert_ne!(code, 0, "{report}");
    assert!(report.contains("open to other users"), "{report}");

    std::fs::remove_dir(&private).expect("rmdir");
    let (_, printed, report) = run();
    let mode = std::fs::metadata(&private).expect("created").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "{report}");
    let placed: Vec<_> = std::fs::read_dir(&private).expect("read").flatten().collect();
    assert!(
        placed.iter().any(|e| e.file_name().to_string_lossy().starts_with("trapped-")),
        "{report}"
    );
    // The guest still sees the name it was started by, not the copy's.
    if let Some(line) = printed.lines().find(|l| l.starts_with("argv0 ")) {
        assert_eq!(line, format!("argv0 {}", facts.display()), "{report}");
    }
    // Its bundle names the program and holds the copy's digest; a replay traps it again.
    let bundle = tmp.join("trapped.bohr");
    let (code, _, report) = output(
        Command::new(env!("CARGO_BIN_EXE_bohr"))
            .args(["run", "--seed", "1", "--patch-cpuid", "--save"])
            .arg(&bundle)
            .arg("--")
            .arg(&facts)
            .env("TMPDIR", &tmp),
    );
    assert_eq!(code, 0, "{report}");
    let (code, _, report) = output(
        Command::new(env!("CARGO_BIN_EXE_bohr"))
            .args(["replay", "--verify-only"])
            .arg(&bundle)
            .env("TMPDIR", &tmp),
    );
    assert_eq!(code, 0, "{report}");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A run with every descriptor up to the coverage counters' number held open still places the
/// counters and starts the guest (S11).
#[test]
fn the_coverage_counters_never_land_on_a_pipe() {
    let Some(path) = guest("covered_branches") else {
        return;
    };
    let hold = "for fd in $(seq 3 199); do eval \"exec $fd</dev/null\"; done; exec \"$0\" \"$@\"";
    let (code, _, report) = output(Command::new("bash").args([
        "-c",
        hold,
        env!("CARGO_BIN_EXE_bohr"),
        "run",
        "--seed",
        "1",
        "--",
        &path.to_string_lossy(),
    ]));
    assert!(code == 0 || code == 1, "{report}");
    assert!(!report.contains("could not"), "{report}");
}
