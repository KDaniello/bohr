//! What a guest actually gets, as opposed to what the host could have given it.
//!
//! These need a real guest, so they run only where one can be started. A host that cannot trap
//! processor identification is not a reason to skip them: the point is that the backend says so.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use bohr_backend::{DeterminismBackend, GuestSpec, Resume};
use bohr_backend_ptrace::PtraceBackend;
use bohr_types::NodeId;

/// A guest that starts and stops, built by the harness that builds the others.
fn tiny_guest() -> Option<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/guests/build/cat");
    std::path::Path::new(path).exists().then(|| path.to_owned())
}

/// **A run may not claim a trap the guest was never given.** A probe in a throwaway child answers
/// about the host, and `execve` clears the setting, so such a probe would report
/// `determinism: full` for a guest that never had the trap.
#[test]
fn what_the_host_can_do_is_not_what_the_guest_was_given() {
    let Some(program) = tiny_guest() else {
        return;
    };
    let mut backend = PtraceBackend::new();
    let spec = GuestSpec {
        argv: vec![program],
        env: vec![("HOME".to_owned(), "/".to_owned())],
        executable: None,
    };
    match backend.spawn(NodeId::ZERO, &spec) {
        Ok(_) => {}
        // No guest can be started here at all; the claim under test needs one.
        Err(why) => {
            eprintln!("skipped: no guest could be started here: {why}");
            return;
        }
    }

    let trapped = backend.capabilities().traps_cpuid;
    let notes = backend.degradations();
    eprintln!("traps_cpuid={trapped} notes={notes:?}");
    assert_eq!(
        trapped,
        !notes.iter().any(|n| n.starts_with("cpuid_not_trapp")),
        "the capability and the degradation note disagree about the same guest: {notes:?}"
    );
}

/// This process's children running `program`, as host pids, dead or alive but not yet reaped.
fn running(program: &str) -> Vec<String> {
    let me = std::process::id().to_string();
    let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter(|entry| {
            let ours = std::fs::read_to_string(entry.path().join("stat")).is_ok_and(|stat| {
                stat.rsplit(") ").next().unwrap_or("").split(' ').nth(1) == Some(me.as_str())
            });
            let named = std::fs::read(entry.path().join("cmdline"))
                .is_ok_and(|line| String::from_utf8_lossy(&line).contains(program));
            ours && named
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// **A guest alive when its run ends ends with the run, not with the process that ran it.** A
/// command of many runs in one process would otherwise keep every run's survivors.
#[test]
fn a_guest_still_alive_when_the_backend_goes_goes_with_it() {
    ends_with_its_backend("long_sleep", 1, false);
}

/// **A guest of several threads goes too, and the backend does not wait on it forever.** An
/// isolated guest is the init of its pid namespace, and an exiting init waits for its traced
/// threads to be reaped — by the backend that would be waiting for it.
#[test]
fn a_guest_of_several_threads_goes_without_a_wait_on_itself() {
    ends_with_its_backend("threads_outlive_a_kill", 2, false);
}

/// **A node killed and not yet collected goes as well.** A kill forgets its tasks at once and
/// leaves their deaths to the next wait, which a run that ends there never makes.
#[test]
fn a_node_killed_at_the_end_of_its_run_is_reaped_with_it() {
    ends_with_its_backend("threads_outlive_a_kill", 2, true);
}

/// Starts `guest`, runs it until it has `tasks` tasks, kills its node if `killed`, drops the
/// backend, and requires that every process of it is reaped well inside the backend's deadline.
///
/// The host clock is read, which nothing in the engine may do: the claim is about real time.
#[allow(clippy::disallowed_methods)]
fn ends_with_its_backend(guest: &str, tasks: usize, killed: bool) {
    let path = format!("{}/../../testdata/guests/build/{guest}", env!("CARGO_MANIFEST_DIR"));
    assert!(std::path::Path::new(&path).exists(), "{path} is not built: run `make guests`");
    let mut backend = PtraceBackend::new();
    let spec = GuestSpec {
        argv: vec![path],
        env: vec![("HOME".to_owned(), "/".to_owned())],
        executable: None,
    };
    let first = match backend.spawn(NodeId::ZERO, &spec) {
        Ok(first) => first,
        Err(why) => {
            eprintln!("skipped: no guest could be started here: {why}");
            return;
        }
    };
    backend.resume(first, Resume::Continue).expect("a resume");
    let mut seen = std::collections::BTreeSet::from([first]);
    let mut last = first;
    while seen.len() < tasks {
        let stop = backend.next_stop().expect("a stop").expect("the guest ended too soon");
        seen.insert(stop.task);
        last = stop.task;
        backend.resume(stop.task, Resume::Continue).expect("a resume");
    }
    // Read before the kill: a killed task's command line is empty once it is a zombie.
    let program = format!("guests/build/{guest}");
    let before = running(&program);
    assert!(!before.is_empty(), "the guest never started");
    if killed {
        backend.kill(last).expect("a kill");
    }
    let started = std::time::Instant::now();
    drop(backend);
    let took = started.elapsed();
    let left: Vec<&String> = before
        .iter()
        .filter(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
        .collect();
    assert!(left.is_empty(), "the guest outlived its backend, zombies included: {left:?}");
    assert!(took.as_millis() < 500, "the backend took {took:?}, as long as giving up");
}
