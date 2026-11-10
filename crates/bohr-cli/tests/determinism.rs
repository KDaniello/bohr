//! Determinism, checked against real programs under real interception.
//!
//! Linux only; each test skips itself when its guest from `testdata/guests` is not built. The same
//! claims are checked against scripted guests in `bohr-testkit`, which runs anywhere.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{path::PathBuf, process::Command};

/// Repetitions behind goal T1: one process, one seed, every run agreeing.
const REPEATS: &str = "1000";

fn guest(name: &str) -> Option<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("testdata/guests/build")
        .join(name);
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

fn bohr(args: &[&str]) -> (i32, Vec<u8>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary must be runnable");
    (out.status.code().unwrap_or(-1), out.stdout, String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn repeat_1000() {
    let Some(path) = guest("clock_and_random") else {
        return;
    };
    let (code, _, report) = bohr(&["run", "--seed", "42", "--repeat", REPEATS, "--", &path]);
    assert_eq!(code, 0, "{report}");
    assert!(!report.contains("DIVERGED"), "{report}");
}

/// A guest that replaces itself is intercepted as fully afterwards as before.
///
/// Every `execve` gets a fresh auxiliary vector, with the vDSO back and a new `AT_RANDOM`; both are
/// suppressed again, or the second program reads the host clock and its stack moves.
#[test]
fn a_guest_that_replaces_itself_still_repeats() {
    let (Some(launcher), Some(target)) = (guest("becomes_another"), guest("clock_and_random"))
    else {
        return;
    };
    let (code, printed, report) =
        bohr(&["run", "--seed", "42", "--repeat", REPEATS, "--", &launcher, &target]);
    assert_eq!(code, 0, "{report}");
    assert!(!report.contains("DIVERGED"), "{report}");

    // The world's clock starts at zero; the host's would print a date.
    let line = String::from_utf8_lossy(&printed);
    assert!(
        line.starts_with("0000000000000000."),
        "the exec'd guest read a clock that is not the world's: {line}"
    );
}

/// Goal C1: a guest reading an endless source of randomness prints the same bytes every time.
#[test]
fn urandom_stable() {
    let Some(path) = guest("cat") else {
        return;
    };
    let args = ["run", "--seed", "42", "--repeat", REPEATS, "--", &path, "/dev/urandom"];
    let (code, bytes, report) = bohr(&args);
    assert_eq!(code, 0, "{report}");
    assert!(!bytes.is_empty(), "the guest must have written what it read");

    let (_, again, _) = bohr(&args);
    assert_eq!(bytes, again, "two invocations of one seed must put the same bytes on the pipe");

    let mut other = args;
    other[2] = "43";
    let (_, different, _) = bohr(&other);
    assert_ne!(bytes, different, "a different seed must reach different bytes");
}

/// `rdtsc` is answered from logical time like any other clock.
#[test]
fn timestamp_counter_reproduces() {
    let Some(path) = guest("rdtsc_and_clock") else {
        return;
    };
    let (code, bytes, report) = bohr(&["run", "--seed", "42", "--repeat", "100", "--", &path]);
    assert_eq!(code, 0, "{report}");

    let (_, again, _) = bohr(&["run", "--seed", "42", "--repeat", "1", "--", &path]);
    assert_eq!(bytes, again, "the counter must read the same on every run");
}

/// An unmodelled syscall stops the run by name and never reaches the host.
#[test]
fn an_unmodelled_syscall_stops_the_run() {
    let Some(path) = guest("unmodelled_syscall") else {
        return;
    };
    let (code, _, report) = bohr(&["run", "--seed", "1", "--", &path]);
    assert_eq!(code, 3, "{report}");
    assert!(report.contains("BOHR-2001"), "the refusal must carry its code: {report}");
}

/// io_uring has its own code, `BOHR-2004`.
///
/// A ring submits work without a syscall per operation, so there is no table row to add, which is
/// what `BOHR-2001` would suggest.
#[test]
fn io_uring_is_declined_at_the_door_and_refused_past_it() {
    let Some(path) = guest("uses_io_uring") else {
        return;
    };
    // Setup is answered `ENOSYS`, as on a kernel without io_uring, and the guest falls back.
    let (code, _, report) = bohr(&["run", "--seed", "1", "--", &path]);
    assert_eq!(code, 0, "opening a ring must not end the run: {report}");

    let Some(path) = guest("enters_io_uring") else {
        return;
    };
    // Entering a ring ends the run under `BOHR-2004`.
    let (code, _, report) = bohr(&["run", "--seed", "1", "--", &path]);
    assert_eq!(code, 3, "{report}");
    assert!(report.contains("BOHR-2004"), "the refusal must carry its own code: {report}");
    assert!(report.contains("io_uring"), "the refusal must name the mechanism: {report}");
}

/// Two threads through a mutex reproduce, and the interleaving is the seed's to choose.
#[test]
fn threads_reproduce_and_follow_the_seed() {
    let Some(path) = guest("two_threads") else {
        return;
    };
    let (code, order, report) = bohr(&["run", "--seed", "1", "--repeat", "200", "--", &path]);
    assert_eq!(code, 0, "{report}");
    assert!(!order.is_empty(), "the guest must have printed its interleaving");

    let (_, again, _) = bohr(&["run", "--seed", "1", "--", &path]);
    assert_eq!(order, again, "one seed, one interleaving");

    let different = (2..40u32)
        .filter_map(|seed| {
            let (_, out, _) = bohr(&["run", "--seed", &seed.to_string(), "--", &path]);
            (out != order).then_some(out)
        })
        .next();
    assert!(different.is_some(), "no seed produced a different interleaving");
}

/// Goal C2: a guest that waits a day finishes in the time its own code takes.
///
/// The claim is about real time, so this test reads the host clock.
#[allow(clippy::disallowed_methods)]
#[test]
fn a_day_of_sleep_costs_no_real_time() {
    let Some(path) = guest("long_sleep") else {
        return;
    };
    let started = std::time::Instant::now();
    let (code, elapsed, report) = bohr(&["run", "--seed", "1", "--", &path]);
    assert_eq!(code, 0, "{report}");

    // A day give or take the node's drift: 17 seconds at 200 ppm.
    let seen: i64 = String::from_utf8_lossy(&elapsed).trim().parse().expect("a count of seconds");
    let day = 24 * 60 * 60;
    assert!(
        (seen - day).abs() <= day * i64::from(bohr_kernel::MAX_DRIFT_PPM) / 1_000_000 + 1,
        "the guest saw {seen} seconds where it slept for {day}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "a day of virtual time took {:?} of real time",
        started.elapsed()
    );
}

/// Goal C3: a known race is found by searching seeds, well inside the budget.
///
/// The tool knows nothing of the defect; the guest exits non-zero and that is the finding.
#[test]
fn a_known_race_is_found_by_searching_seeds() {
    let Some(path) = guest("race_counter") else {
        return;
    };
    let found = (1..=100u32).find(|seed| {
        let (code, _, _) = bohr(&["run", "--seed", &seed.to_string(), "--", &path]);
        code == 1
    });
    assert!(found.is_some(), "the race went unfound in 100 seeds");
}

/// A process that fails from a thread other than its first is a finding, on every run alike.
///
/// Each thread reports its own end in host order; the run waits for the group's status.
#[test]
fn a_failure_from_another_thread_is_found_on_every_run() {
    let Some(path) = guest("exits_from_a_thread") else {
        return;
    };
    let (code, _, report) = bohr(&["run", "--seed", "1", "--repeat", "100", "--", &path]);
    assert_eq!(code, 1, "{report}");
    assert!(report.contains("exit status 3"), "{report}");
}

/// A timer armed for one futex wait ends that wait and no later one, alone and in a cluster.
#[test]
fn a_timed_wait_woken_early_leaves_no_timer_behind() {
    let Some(path) = guest("rewaits_after_a_timed_wait") else {
        return;
    };
    let (code, _, report) = bohr(&["run", "--seed", "1", "--repeat", "20", "--", &path]);
    assert_eq!(code, 0, "{report}");
    let cluster = write_scenario(
        "rewaits",
        &format!(
            "[cluster]
nodes = 1
argv = [\"{path}\"]
[run]
horizon_ticks = 20_000_000_000
"
        ),
    );
    let (code, _, report) = bohr(&["run", "--seed", "1", "--config", &cluster, "--", &path]);
    assert_eq!(code, 0, "{report}");
}

/// A fault point is armed by the run's draw: always at 1/1, never when unlisted, on some seeds at
/// 1/2, and the same on every repeat of one seed.
#[test]
fn a_fault_point_is_armed_by_the_run_and_repeats() {
    let Some(path) = guest("reads_its_armed_fault_points") else {
        return;
    };
    let scenario = |odds: &str, listed: bool| {
        let points = if listed {
            "fault_points = [\"skip_sync\"]
"
        } else {
            ""
        };
        write_scenario(
            &format!("armed-{}-{listed}", odds.replace('/', "-")),
            &format!(
                "[cluster]
nodes = 1
argv = [\"{path}\"]
[faults]
{points}                 fault_point_probability = \"{odds}\"
"
            ),
        )
    };
    let status = |file: &str, seed: u32| {
        let (code, _, report) =
            bohr(&["run", "--seed", &seed.to_string(), "--config", file, "--", &path]);
        (code, report.contains("exit status 7"))
    };
    let always = scenario("1/1", true);
    assert_eq!(status(&always, 1), (1, true), "armed at certain odds");
    let unlisted = scenario("1/1", false);
    assert_eq!(status(&unlisted, 1), (0, false), "nothing listed, nothing armed");
    let even = scenario("1/2", true);
    let armed: Vec<bool> = (1..=20).map(|seed| status(&even, seed).1).collect();
    assert!(armed.iter().any(|a| *a) && armed.iter().any(|a| !*a), "{armed:?}");
    let (code, _, report) =
        bohr(&["run", "--seed", "3", "--repeat", "20", "--config", &even, "--", &path]);
    assert!(!report.contains("DIVERGED"), "{report}");
    assert!(code == 0 || code == 1, "{report}");
}

/// A file survives being unlinked while open, as temporary files and databases expect.
#[test]
fn files_round_trip_and_reproduce() {
    let Some(path) = guest("file_roundtrip") else {
        return;
    };
    let (code, _, report) = bohr(&["run", "--seed", "5", "--repeat", "100", "--", &path]);
    assert_eq!(code, 0, "{report}");
}

/// A file reached through a mapping agrees with the same file reached through its descriptor.
///
/// Execution clients read their chain store through mappings. The guest checks both directions
/// and its exit status names the one that failed.
#[test]
fn a_mapped_file_and_its_descriptor_agree() {
    let Some(path) = guest("maps_a_file") else {
        return;
    };
    let (code, _, report) = bohr(&["run", "--seed", "5", "--repeat", "100", "--", &path]);
    assert_eq!(code, 0, "{report}");
}

/// A thread that gives up its turn gets another one, whatever the policy does with it in between.
///
/// A starting runtime looks like this: an idle worker yields while another thread sets up. The
/// guest exits non-zero if the yielding thread never runs again.
#[test]
fn a_thread_that_yields_is_scheduled_again() {
    let Some(path) = guest("one_yields_one_spins") else {
        return;
    };
    for policy in ["pct", "random"] {
        let (code, _, report) = bohr(&["run", "--seed", "7", "--scheduler", policy, "--", &path]);
        assert_eq!(code, 0, "under {policy}: {report}");
    }
}

/// A guest that faults ends, and ending that way is a finding.
///
/// A swallowed fault re-runs the instruction forever, so the host clock bounds the test.
#[allow(clippy::disallowed_methods)]
#[test]
fn a_guest_that_faults_is_reported_as_one() {
    let Some(path) = guest("segfault") else {
        return;
    };
    let started = std::time::Instant::now();
    let (code, _, report) = bohr(&["run", "--seed", "1", "--", &path]);
    assert_eq!(code, 1, "a crash is a finding: {report}");
    assert!(report.contains("signal 11"), "the report must name the signal: {report}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "the fault was not acted on, it was repeated"
    );
}

/// Writes a configuration file and returns its path.
fn write_scenario(name: &str, body: &str) -> String {
    let dir = std::env::temp_dir().join("bohr-scenario-test");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write scenario");
    path.to_string_lossy().into_owned()
}

/// The per-commit guest set from `docs/TESTING.md` §7.1, each under real interception.
///
/// One test with one assertion; a failure names its guest.
#[test]
fn the_per_commit_guests_reproduce() {
    // `socketpair_pingpong` needs sockets, which only the cluster world has.
    let scenario = write_scenario(
        "per-commit.toml",
        "[cluster]
nodes = 1
argv = []
",
    );
    for name in ["threads_racing", "spawn_and_wait", "file_write_sync", "socketpair_pingpong"] {
        let Some(path) = guest(name) else {
            continue;
        };
        let mut args =
            vec!["run", "--seed", "5", "--repeat", "200", "--config", &scenario, "--", &path];
        if name != "socketpair_pingpong" {
            args.drain(5..7);
        }
        let (code, out, report) = bohr(&args);
        assert!(code == 0 || code == 4, "{name}: {report}");
        assert!(!report.contains("DIVERGED"), "{name}: {report}");
        assert!(!out.is_empty(), "{name} printed nothing, so nothing was compared");

        // A second process must print the same bytes.
        let mut once = args.clone();
        once.drain(3..5);
        let (_, again, _) = bohr(&once);
        assert_eq!(out, again, "{name} printed different bytes on a second invocation");
    }
}

/// A scenario file drives the run: the world configuration parses, applies and reproduces.
#[test]
fn a_scenario_file_configures_the_run() {
    let Some(path) = guest("file_write_sync") else {
        return;
    };
    let dir = std::env::temp_dir().join("bohr-scenario-test");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let config = dir.join("scenario.toml");
    std::fs::write(
        &config,
        r#"
        [cluster]
        nodes = 1
        argv = []

        [network]
        latency_mean_ticks = 500_000

        [disk]
        stall_probability = "1/4"
        stall_mean_ticks = 200_000
        "#,
    )
    .expect("write scenario");
    let config = config.to_string_lossy().into_owned();

    let args = ["run", "--seed", "3", "--repeat", "50", "--config", &config, "--", &path];
    let (code, out, report) = bohr(&args);
    assert!(code == 0 || code == 4, "{report}");
    assert!(!report.contains("DIVERGED"), "{report}");
    assert!(!out.is_empty(), "the guest printed nothing under the scenario");
}

/// Several real programs run as one deterministic cluster, each traced in namespaces of its own.
#[test]
fn a_multi_node_cluster_of_real_programs_reproduces() {
    let Some(path) = guest("startup") else {
        return;
    };
    let body = format!("[cluster]\nnodes = 3\nargv = [\"{path}\", \"{{node}}\"]\n");
    let config = write_scenario("cluster.toml", &body);

    let (code, _, report) =
        bohr(&["run", "--seed", "1", "--repeat", "20", "--config", &config, "--", &path]);
    assert!(code == 0 || code == 4, "three nodes must run as one world: {report}");
    assert!(!report.contains("DIVERGED"), "{report}");
}

/// Three waits run and repeat: a signal to another thread runs its handler, a timed wait times out,
/// and an untimed poll on a ready descriptor returns.
#[test]
fn what_was_refused_on_purpose_now_runs_and_repeats() {
    for (name, says) in [
        ("signals_a_thread", "handled by the waiter"),
        ("waits_with_a_timeout", "timed out"),
        ("polls_forever", "ready"),
    ] {
        let Some(path) = guest(name) else {
            continue;
        };
        let (code, out, report) = bohr(&["run", "--seed", "3", "--repeat", "100", "--", &path]);
        assert_eq!(code, 0, "{name}: {report}");
        assert!(!report.contains("DIVERGED"), "{name}: {report}");
        assert!(String::from_utf8_lossy(&out).contains(says), "{name} said: {report}");
    }
}
