//! What a guest actually gets, as opposed to what the host could have given it.
//!
//! These need a real guest, so they run only where one can be started. A host that cannot trap
//! processor identification is not a reason to skip them: the point is that the backend says so.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use bohr_backend::{DeterminismBackend, GuestSpec};
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
    let spec = GuestSpec { argv: vec![program], env: vec![("HOME".to_owned(), "/".to_owned())] };
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
