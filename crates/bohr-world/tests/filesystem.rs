//! What the filesystem a guest starts on will and will not do.
//!
//! A file cannot be made where its directory is not. The hierarchy standard's directories, such as
//! `/tmp`, exist from the start; nothing else does until made.

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
    answers_with(paths, &[], ops)
}

/// [`answers`], with `bytes` written at their addresses beside the paths.
fn answers_with(
    paths: &[&str],
    bytes: &[(u64, Vec<u8>)],
    ops: Vec<GuestOp>,
) -> Result<Vec<(u32, i64)>, KernelError> {
    let tape = Tape::from_seed(Seed(1));
    let mut backend = NullBackend::new(NullProgram::single(ops));
    for (index, path) in paths.iter().enumerate() {
        let mut bytes = path.as_bytes().to_vec();
        bytes.push(0);
        backend.preload(GuestAddr(slot(index as u64)), &bytes).expect("the slot is in range");
    }
    for (at, held) in bytes {
        backend.preload(GuestAddr(*at), held).expect("in range");
    }
    let mut world = MinimalWorld::new();
    let mut policy = RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());

    run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
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
/// Both halves are checked.
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
    // `EBADF`, since the caller gave a descriptor.
    assert_eq!(answered[2], (FSTATFS, EBADF), "a descriptor nobody holds is a bad descriptor");
}

const RENAME: u32 = 82;
const RMDIR: u32 = 84;
const UNLINK: u32 = 87;
const EINVAL: i64 = -22;
const ENOTEMPTY: i64 = -39;

fn rename(from: u64, to: u64) -> GuestOp {
    GuestOp::Syscall { nr: RENAME, args: [slot(from), slot(to), 0, 0, 0, 0] }
}

fn rmdir(index: u64) -> GuestOp {
    GuestOp::Syscall { nr: RMDIR, args: [slot(index), 0, 0, 0, 0, 0] }
}

fn unlink(index: u64) -> GuestOp {
    GuestOp::Syscall { nr: UNLINK, args: [slot(index), 0, 0, 0, 0, 0] }
}

/// A directory with something in it is not removed, and is once it is empty.
#[test]
fn a_directory_goes_only_once_it_is_empty() {
    let answered = answers(
        &["/tmp/d", "/tmp/d/f"],
        vec![mkdir(0), open(1, CREATE), rmdir(0), stat(1), unlink(1), rmdir(0), stat(0)],
    )
    .expect("nothing here is refused");
    let answers: Vec<i64> = answered.iter().map(|(_, ret)| *ret).collect();
    assert_eq!(answers[2..], [ENOTEMPTY, 0, 0, 0, ENOENT], "{answered:?}");
}

/// A directory moves with everything under it.
#[test]
fn a_directory_moves_with_everything_under_it() {
    let answered = answers(
        &["/tmp/a", "/tmp/a/f", "/tmp/b", "/tmp/b/f"],
        vec![mkdir(0), open(1, CREATE), rename(0, 2), stat(3), stat(1), stat(0)],
    )
    .expect("nothing here is refused");
    let answers: Vec<i64> = answered.iter().map(|(_, ret)| *ret).collect();
    assert_eq!(answers[2..], [0, 0, ENOENT, ENOENT], "{answered:?}");
}

/// A directory never moves into itself, replaces only an empty directory, and a file and a
/// directory never replace each other.
#[test]
fn a_directory_moves_only_where_linux_lets_it() {
    const ENOTDIR: i64 = -20;
    let answered = answers(
        &["/tmp/a", "/tmp/a/sub", "/tmp/b", "/tmp/b/f", "/tmp/file"],
        vec![
            mkdir(0),
            mkdir(2),
            open(3, CREATE),
            open(4, CREATE),
            rename(0, 1),
            rename(0, 2),
            rename(0, 4),
            rename(4, 2),
            unlink(3),
            rename(0, 2),
            stat(2),
            stat(0),
        ],
    )
    .expect("nothing here is refused");
    let answers: Vec<i64> = answered.iter().map(|(_, ret)| *ret).collect();
    assert_eq!(answers[4..], [EINVAL, ENOTEMPTY, ENOTDIR, EISDIR, 0, 0, 0, ENOENT], "{answered:?}");
}

/// An appending write lands at the end and leaves the position after it; `pwrite` on an
/// appending descriptor lands at the end too, as Linux makes it.
#[test]
fn an_appending_write_leaves_the_position_at_the_end() {
    const WRITE: u32 = 1;
    const CLOSE: u32 = 3;
    const LSEEK: u32 = 8;
    const PWRITE64: u32 = 18;
    const O_APPEND: u64 = 0o2000;
    const SEEK_CUR: u64 = 1;
    const SEEK_END: u64 = 2;
    // The path's own bytes are what is written: ten, then five, then five.
    let answered = answers(
        &["/tmp/log.0000"],
        vec![
            open(0, CREATE),
            GuestOp::Syscall { nr: WRITE, args: [3, slot(0), 10, 0, 0, 0] },
            GuestOp::Syscall { nr: CLOSE, args: [3, 0, 0, 0, 0, 0] },
            open(0, CREATE | O_APPEND),
            GuestOp::Syscall { nr: WRITE, args: [3, slot(0), 5, 0, 0, 0] },
            GuestOp::Syscall { nr: LSEEK, args: [3, 0, SEEK_CUR, 0, 0, 0] },
            GuestOp::Syscall { nr: PWRITE64, args: [3, slot(0), 5, 0, 0, 0] },
            GuestOp::Syscall { nr: LSEEK, args: [3, 0, SEEK_CUR, 0, 0, 0] },
            GuestOp::Syscall { nr: LSEEK, args: [3, 0, SEEK_END, 0, 0, 0] },
        ],
    )
    .expect("nothing here is refused");
    let answers: Vec<i64> = answered.iter().map(|(_, ret)| *ret).collect();
    assert_eq!(answers, [3, 10, 0, 3, 5, 15, 5, 15, 20], "{answered:?}");
}

/// A link is a name: it is renamed over another, as an atomic switch does, and unlinked.
#[test]
fn a_link_is_renamed_and_unlinked_as_a_name() {
    const SYMLINK: u32 = 88;
    const READLINK: u32 = 89;
    let link = |target: u64, at: u64| GuestOp::Syscall {
        nr: SYMLINK,
        args: [slot(target), slot(at), 0, 0, 0, 0],
    };
    let read = |at: u64| GuestOp::Syscall { nr: READLINK, args: [slot(at), SCRATCH, 64, 0, 0, 0] };
    let answered = answers(
        &["/tmp/release-1", "/tmp/release-22", "/tmp/current", "/tmp/current.next"],
        vec![link(0, 2), link(1, 3), rename(3, 2), read(2), read(3), unlink(2), read(2)],
    )
    .expect("nothing here is refused");
    let answers: Vec<i64> = answered.iter().map(|(_, ret)| *ret).collect();
    let next = "/tmp/release-22".len() as i64;
    assert_eq!(answers, [0, 0, 0, next, ENOENT, 0, ENOENT], "{answered:?}");
}

/// An empty segment in a gathering write is passed over, as Linux passes it: the bytes after it
/// are written too.
#[test]
fn a_gathering_write_passes_over_an_empty_segment() {
    const WRITEV: u32 = 20;
    let iov = SCRATCH + 0x100;
    let mut vector = Vec::new();
    for (base, len) in [(slot(1), 5u64), (slot(2), 0), (slot(3), 5)] {
        vector.extend_from_slice(&base.to_le_bytes());
        vector.extend_from_slice(&len.to_le_bytes());
    }
    let answered = answers_with(
        &["/tmp/joined", "hello", "", "world"],
        &[(iov, vector)],
        vec![open(0, CREATE), GuestOp::Syscall { nr: WRITEV, args: [3, iov, 3, 0, 0, 0] }],
    )
    .expect("a run");
    assert_eq!(answered[1], (WRITEV, 10), "{answered:?}");
}
