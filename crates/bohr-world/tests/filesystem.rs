//! What the filesystem a guest starts on will and will not do.
//!
//! Two claims, and they pull against each other on purpose.
//!
//! **A file cannot be made where its directory is not.** Waving that through would let a guest that
//! skipped its own `mkdir` open a file the real filesystem would have refused it, and then be
//! judged on a path it could never have taken. A model that produces states the real thing cannot
//! does not miss defects — it invents them.
//!
//! **And yet the ordinary thing must work.** Every filesystem a program is ever started on already
//! has `/tmp`, so a program writing there without making it first is not skipping anything. A world
//! that refused *that* would refuse an open which succeeds everywhere, and a correct program would
//! look broken for a reason belonging entirely to this model.
//!
//! Between them sits the rule: the hierarchy standard's directories are there from the start, and
//! nothing else is until somebody makes it.

use bohr_backend::GuestSpec;
use bohr_backend_null::{GuestOp, MEMORY_BASE, NullBackend, NullProgram};
use bohr_kernel::{
    KernelError, Oracle, RandomPolicy, RunConfig, RunInputs, RunOutcome, Verdict, run,
};
use bohr_tape::Tape;
use bohr_trace::{EventKind, TraceWriter, read_events};
use bohr_types::{GuestAddr, OracleId, Seed};
use bohr_world::MinimalWorld;

/// Judges nothing: what is under test is what the world answered.
struct AcceptEverything;

impl Oracle for AcceptEverything {
    fn identity(&self) -> OracleId {
        OracleId::new("accept-everything", 1)
    }

    fn judge(&mut self, _outcome: &RunOutcome) -> Verdict {
        Verdict::Clean
    }
}

const OPENAT: u32 = 257;
const MKDIR: u32 = 83;
const NEWFSTATAT: u32 = 262;
const FSTATFS: u32 = 138;

/// `O_CREAT | O_RDWR`, and the same read-only.
const CREATE: u64 = 0o100 | 2;
const READ_ONLY: u64 = 0;
/// `O_DIRECTORY`.
const MUST_BE_A_DIRECTORY: u64 = 0o200_000;

const ENOENT: i64 = -2;
const EBADF: i64 = -9;
const EEXIST: i64 = -17;
const ENOTDIR: i64 = -20;
const EISDIR: i64 = -21;

/// Where each path is preloaded, one every sixty-four bytes.
fn slot(index: u64) -> u64 {
    MEMORY_BASE + index * 0x40
}

/// Somewhere to write a structure the guest never reads back.
const SCRATCH: u64 = MEMORY_BASE + 0x800;

/// Runs a script against a fresh world and reports what each call answered, in order.
fn answers(paths: &[&str], ops: Vec<GuestOp>) -> Result<Vec<(u32, i64)>, KernelError> {
    let tape = Tape::from_seed(Seed(1));
    let mut backend = NullBackend::new(NullProgram::single(ops));
    for (index, path) in paths.iter().enumerate() {
        let mut bytes = path.as_bytes().to_vec();
        bytes.push(0);
        backend.preload(GuestAddr(slot(index as u64)), &bytes).expect("the slot is in range");
    }
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
    Ok(read_events(&log)
        .expect("the log decodes")
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Syscall { nr, ret, .. } => Some((nr, ret)),
            _ => None,
        })
        .collect())
}

fn open(index: u64, flags: u64) -> GuestOp {
    GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, slot(index), flags, 0o644, 0, 0] }
}

fn mkdir(index: u64) -> GuestOp {
    GuestOp::Syscall { nr: MKDIR, args: [slot(index), 0o755, 0, 0, 0, 0] }
}

fn stat(index: u64) -> GuestOp {
    GuestOp::Syscall { nr: NEWFSTATAT, args: [u64::MAX, slot(index), SCRATCH, 0, 0, 0] }
}

/// A file cannot be made where its directory is not, and can be once it is.
///
/// The pair is the test. Only the first half would also pass on a model that refused everything,
/// and only the second on the flat namespace this replaced.
#[test]
fn a_file_needs_the_directory_over_it() {
    let answered = answers(
        &["/nowhere/store.db", "/nowhere"],
        vec![open(0, CREATE), mkdir(1), open(0, CREATE)],
    )
    .expect("nothing here is refused, only answered");

    assert_eq!(
        answered[0],
        (OPENAT, ENOENT),
        "a file under a directory nobody made is a file the real filesystem would refuse"
    );
    assert_eq!(answered[1], (MKDIR, 0), "and making the directory is how a guest fixes that");
    assert!(answered[2].1 >= 0, "after which the same open succeeds: {:?}", answered[2]);
}

/// The directories every filesystem already has are already here, and cannot be made twice.
#[test]
fn the_ordinary_places_are_there_from_the_start() {
    let answered = answers(
        &["/tmp/scratch", "/tmp", "/var/tmp/scratch"],
        vec![open(0, CREATE), mkdir(1), open(2, CREATE)],
    )
    .expect("nothing here is refused");

    assert!(answered[0].1 >= 0, "writing to /tmp needs no ceremony: {:?}", answered[0]);
    assert_eq!(answered[1], (MKDIR, EEXIST), "and /tmp cannot be made a second time");
    assert!(answered[2].1 >= 0, "nor does a nested one the standard names: {:?}", answered[2]);
}

/// A directory and a file are told apart, in both directions and by both callers who ask.
#[test]
fn a_directory_is_not_a_file_and_says_so() {
    let answered = answers(
        &["/tmp", "/tmp/store.db", "/nowhere"],
        vec![
            open(0, CREATE),
            open(0, READ_ONLY),
            open(1, CREATE),
            open(1, READ_ONLY | MUST_BE_A_DIRECTORY),
            stat(0),
            stat(2),
        ],
    )
    .expect("nothing here is refused");

    assert_eq!(answered[0], (OPENAT, EISDIR), "a directory refuses an open that would write");
    assert!(answered[1].1 >= 0, "and accepts one that only reads: {:?}", answered[1]);
    assert!(answered[2].1 >= 0, "a file under it is made as usual: {:?}", answered[2]);
    assert_eq!(answered[3], (OPENAT, ENOTDIR), "and is not a directory however it is asked for");
    assert_eq!(answered[4], (NEWFSTATAT, 0), "a name that is there can be asked about");
    assert_eq!(answered[5], (NEWFSTATAT, ENOENT), "and one that is not, cannot");
}

/// The filesystem answers for itself, and only about descriptors that name something.
#[test]
fn the_filesystem_reports_its_own_numbers() {
    let answered = answers(
        &["/tmp/store.db"],
        vec![
            open(0, CREATE),
            GuestOp::Syscall { nr: FSTATFS, args: [3, SCRATCH, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: FSTATFS, args: [77, SCRATCH, 0, 0, 0, 0] },
        ],
    )
    .expect("nothing here is refused");

    assert!(answered[0].1 >= 0, "the file opens: {:?}", answered[0]);
    assert_eq!(answered[1], (FSTATFS, 0), "and the filesystem under it answers");
    // `EBADF`, not `ENOENT`: the caller gave a descriptor, not a path, so a missing name is not
    // an answer to what it asked. The wrong one was pinned here.
    assert_eq!(answered[2], (FSTATFS, EBADF), "a descriptor nobody holds is a bad descriptor");
}
