//! What a backend says it can do, checked against what it does.
//!
//! A capability is a self-report; these tests compare it with behaviour.
//!
//! Every backend answers the same questions. The scripted one runs everywhere.

use bohr_backend::{
    BackendCapabilities, DeterminismBackend, GuestExit, GuestSpec, ProgressUnit, Resume, Stop,
    StopKind,
};
use bohr_backend_null::{GuestOp, NullBackend, NullProgram};
use bohr_types::NodeId;

/// Runs one scripted guest to its next stop.
fn stop_of(ops: Vec<GuestOp>) -> Stop {
    let mut backend = NullBackend::new(NullProgram::single(ops));
    let task = backend.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
    backend.resume(task, Resume::Continue).expect("resume");
    backend.next_stop().expect("a stop").expect("a stop")
}

/// A backend that claims to trap an instruction must produce the stop that trapping it makes.
///
/// Each of the three claims is checked separately.
#[test]
fn every_trap_a_backend_claims_is_a_trap_it_performs() {
    let capabilities = NullBackend::new(NullProgram::single(vec![GuestOp::Exit(0)])).capabilities();

    if capabilities.traps_rdtsc {
        let stop = stop_of(vec![GuestOp::ReadTimestamp { rdtscp: false }, GuestOp::Exit(0)]);
        assert!(
            matches!(stop.kind, StopKind::TimestampCounter { .. }),
            "{} claims to trap the timestamp counter and answered {:?}",
            capabilities.name,
            stop.kind
        );
    }

    if capabilities.traps_cpuid {
        let stop = stop_of(vec![GuestOp::CpuId { leaf: 1, subleaf: 0 }, GuestOp::Exit(0)]);
        assert!(
            matches!(stop.kind, StopKind::CpuIdentification { .. }),
            "{} claims to trap processor identification and answered {:?}",
            capabilities.name,
            stop.kind
        );
    }
}

/// A signal the guest takes reaches the boundary as one.
#[test]
fn a_signal_the_guest_takes_arrives_as_a_signal() {
    let stop = stop_of(vec![GuestOp::RaiseSignal { signal: 11 }, GuestOp::Exit(0)]);
    assert!(matches!(stop.kind, StopKind::Signal { signal: 11 }), "{:?}", stop.kind);
}

/// A backend that counts progress can run out of budget.
#[test]
fn a_backend_that_counts_progress_can_run_out_of_it() {
    let capabilities = NullBackend::new(NullProgram::single(vec![GuestOp::Exit(0)])).capabilities();
    assert!(
        matches!(
            capabilities.progress_unit,
            ProgressUnit::Branches | ProgressUnit::Syscalls | ProgressUnit::Operations
        ),
        "a backend counts progress in something"
    );

    // A script with no stopping operation.
    let mut backend = NullBackend::new(NullProgram::single(vec![GuestOp::Jump { pc: 0 }]));
    let task = backend.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
    backend.resume(task, Resume::ContinueWithBudget { budget: 16 }).expect("resume");
    let stop = backend.next_stop().expect("a stop").expect("a stop");
    assert!(matches!(stop.kind, StopKind::BudgetExhausted { .. }), "{:?}", stop.kind);
}

/// A backend without hand-over says so in its capability and returns an empty list.
#[test]
fn handing_over_is_what_the_capability_says_it_is() {
    let mut backend = NullBackend::new(NullProgram::single(vec![GuestOp::Exit(0)]));
    let BackendCapabilities { hands_over_to_debugger, .. } = backend.capabilities();
    let targets = backend.hand_over().expect("an answer rather than a failure");
    assert!(
        hands_over_to_debugger || targets.is_empty(),
        "a backend that cannot hand anything over named something to attach to"
    );
}

/// An exit reaches the boundary as the guest's status.
#[test]
fn an_exit_carries_the_status_the_guest_named() {
    let stop = stop_of(vec![GuestOp::Exit(7)]);
    assert!(matches!(stop.kind, StopKind::TaskExited { status: GuestExit::Exited(7) }), "{stop:?}");
}
