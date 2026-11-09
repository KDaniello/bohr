//! A guest that names sizes no machine could serve.
//!
//! The supervisor is the single component whose failure destroys the evidence a run exists to
//! produce, so a guest must never be able to bring it down by asking for something absurd — and a
//! guest that asks is usually not a hostile one, but a program in the middle of going wrong, which
//! is exactly when the evidence matters. The rule is the same in every case here: a request past
//! what can be held is answered, never obeyed. See `docs/ARCHITECTURE.md` §5.7.

use bohr_backend::{BackendError, GuestSpec};
use bohr_backend_null::{GuestOp, MEMORY_BASE, NullBackend, NullProgram};
use bohr_kernel::{
    KernelError, Oracle, RandomPolicy, RunConfig, RunInputs, RunOutcome, Verdict, run,
};
use bohr_tape::Tape;
use bohr_trace::{Event, EventKind, TraceWriter, read_events};
use bohr_types::{GuestAddr, OracleId, Seed};
use bohr_world::MinimalWorld;

/// Judges nothing: what is under test is that the run ends at all.
struct AcceptEverything;

impl Oracle for AcceptEverything {
    fn identity(&self) -> OracleId {
        OracleId::new("accept-everything", 1)
    }

    fn judge(&mut self, _outcome: &RunOutcome) -> Verdict {
        Verdict::Clean
    }
}

const WRITE: u32 = 1;
const POLL: u32 = 7;
const PWRITE64: u32 = 18;
const FTRUNCATE: u32 = 77;
const OPENAT: u32 = 257;
const GETRANDOM: u32 = 318;

/// Where the path a test opens is placed.
const PATH_AT: u64 = MEMORY_BASE + 0x800;

/// `O_CREAT | O_RDWR`.
const OPEN_FLAGS: u64 = 0o100 | 2;

/// The largest transfer the world will carry in one call.
const MAX_TRANSFER: usize = 1 << 20;

/// Runs one scripted guest and returns what the world answered each call.
fn answers(ops: Vec<GuestOp>) -> Vec<i64> {
    outcome(ops).expect("a scripted guest always completes")
}

/// Runs one scripted guest and returns the answers, or why the run could not go on.
fn outcome(ops: Vec<GuestOp>) -> Result<Vec<i64>, KernelError> {
    let tape = Tape::from_seed(Seed(1));
    let mut backend = NullBackend::new(NullProgram::single(ops));
    backend.preload(GuestAddr(PATH_AT), b"/scratch\0").expect("the path fits");
    let mut world = MinimalWorld::new();
    let mut policy = RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());

    run(RunInputs {
        tape: &tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: std::slice::from_ref(&GuestSpec::default()),
        config: RunConfig::default(),
    })?;

    let (log, _, _) = sink.finish().expect("the sink closes");
    let answers = read_events(&log)
        .expect("the log decodes")
        .iter()
        .filter_map(|event: &Event| match event.kind {
            EventKind::Syscall { ret, .. } => Some(ret),
            _ => None,
        })
        .collect();
    Ok(answers)
}

/// Opens the scratch file, then whatever the test is about.
fn opening(mut rest: Vec<GuestOp>) -> Vec<GuestOp> {
    let mut ops =
        vec![GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PATH_AT, OPEN_FLAGS, 0o644, 0, 0] }];
    ops.append(&mut rest);
    ops.push(GuestOp::Exit(0));
    ops
}

/// A request for more randomness than any machine has is cut to the transfer bound before
/// anything is drawn for it.
///
/// The scripted guest has sixty-four kilobytes to its name, so a bounded request still lands
/// outside it — and the size named in that refusal is the evidence: it is the bound, not the
/// number the guest asked for.
#[test]
fn randomness_beyond_any_buffer_is_cut_to_the_bound() {
    let err = outcome(vec![
        GuestOp::Syscall { nr: GETRANDOM, args: [MEMORY_BASE, u64::MAX, 0, 0, 0, 0] },
        GuestOp::Exit(0),
    ])
    .expect_err("the buffer named is not the guest's to write");

    match err {
        KernelError::Backend(BackendError::BadAddress { len, .. }) => {
            assert_eq!(len, MAX_TRANSFER, "the request reached memory uncut");
        }
        other => panic!("the refusal must name the access: {other:?}"),
    }
}

/// A write far past the end of a file asks for the gap to be allocated, and is refused.
#[test]
fn a_write_past_what_may_be_held_is_refused() {
    let rets = answers(opening(vec![GuestOp::Syscall {
        nr: PWRITE64,
        args: [3, MEMORY_BASE, 8, u64::MAX / 2, 0, 0],
    }]));
    assert_eq!(rets.get(1).copied(), Some(-27), "the write must be EFBIG: {rets:?}");
}

/// The same rule reached the other way: setting a length rather than writing at one.
#[test]
fn a_length_past_what_may_be_held_is_refused() {
    let rets =
        answers(opening(vec![GuestOp::Syscall { nr: FTRUNCATE, args: [3, 1 << 50, 0, 0, 0, 0] }]));
    assert_eq!(rets.get(1).copied(), Some(-27), "the truncation must be EFBIG: {rets:?}");
}

/// An address that wraps the address space is not an address, and walking an array from one must
/// not be arrived at by arithmetic.
#[test]
fn an_array_that_wraps_the_address_space_is_refused() {
    let rets = answers(vec![
        GuestOp::Syscall { nr: POLL, args: [u64::MAX - 4, 64, 0, 0, 0, 0] },
        GuestOp::Exit(0),
    ]);
    assert_eq!(rets.first().copied(), Some(-14), "the walk must be EFAULT: {rets:?}");
}

/// What is refused must still be reachable: the bound is a ceiling, not a closed door.
#[test]
fn a_write_within_the_bound_is_carried_out() {
    let rets = answers(opening(vec![
        GuestOp::Syscall { nr: WRITE, args: [3, MEMORY_BASE, 8, 0, 0, 0] },
        GuestOp::Syscall { nr: FTRUNCATE, args: [3, 4096, 0, 0, 0, 0] },
    ]));
    assert_eq!(rets.get(1).copied(), Some(8), "the write must be taken: {rets:?}");
    assert_eq!(rets.get(2).copied(), Some(0), "the truncation must be taken: {rets:?}");
}
