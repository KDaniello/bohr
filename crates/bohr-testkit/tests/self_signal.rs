//! A guest that kills itself is a crash, not a clean exit.
//!
//! `abort`, a failed assertion in C, a panic configured to abort — all of them end a task by
//! sending it a fatal signal rather than by reaching an exit. The task never *stops on* a signal,
//! so nothing the kernel observes distinguishes it from a task that returned zero; only the world,
//! which answered the call, knows what took it.
//!
//! Recorded as its own test because the failure is silent in the worst way: the run reports clean,
//! the exit status reads zero, and a campaign of a million of them says a guest that aborted every
//! time never once misbehaved.

use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{judge_scenario, Scenario};
use bohr_trace::Outcome;
use bohr_types::Seed;

/// `tgkill`.
const TGKILL: u32 = 234;
/// `write`.
const WRITE: u32 = 1;

/// `SIGABRT`, which is what `abort` raises.
const SIGABRT: u64 = 6;
/// `SIGTERM`, which this world does not treat as fatal.
const SIGTERM: u64 = 15;

const PAYLOAD_AT: u64 = MEMORY_BASE;

/// Says something, then sends itself `signal`.
fn signals_itself(signal: u64) -> NullProgram {
    NullProgram::single(vec![
        GuestOp::Syscall { nr: WRITE, args: [1, PAYLOAD_AT, 4, 0, 0, 0] },
        GuestOp::Syscall { nr: TGKILL, args: [1, 1, signal, 0, 0, 0] },
        GuestOp::Syscall { nr: WRITE, args: [1, PAYLOAD_AT, 4, 0, 0, 0] },
    ])
}

fn judge(program: NullProgram) -> Outcome {
    judge_scenario(
        vec![program],
        &[(0, PAYLOAD_AT, b"done".to_vec())],
        Scenario::default(),
        &Tape::from_seed(Seed(1)),
        RunConfig::default(),
    )
    .report
    .outcome
}

/// The claim. A fatal signal a task sends itself ends the run as a crash.
#[test]
fn a_task_that_aborts_itself_is_a_crash() {
    match judge(signals_itself(SIGABRT)) {
        Outcome::GuestCrashed(why) => {
            assert!(why.contains('6'), "the verdict must name the signal: {why}");
        }
        other => panic!("a guest that aborted was judged {other:?}"),
    }
}

/// And a signal this world does not treat as fatal changes nothing, so the pair means something.
#[test]
fn a_signal_that_is_not_fatal_leaves_the_run_alone() {
    assert_eq!(judge(signals_itself(SIGTERM)), Outcome::Clean);
}
