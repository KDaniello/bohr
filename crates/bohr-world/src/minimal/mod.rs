use std::collections::{BTreeMap, BTreeSet};

mod clock;
mod io_uring;
mod mappings;
mod process;
mod signals;
mod vfs;

use bohr_backend::{Resume, Stop, StopKind};
use bohr_kernel::{
    DeadlineCtx, DueEffects, Effects, KernelError, NodeOp, StopCtx, TaskState, Wake, WorldModel,
};
use bohr_tape::DrawSite;
use bohr_trace::{EventKind, short_digest};
use bohr_types::{GuestAddr, SyncWordId, TaskId, Tick, WorldModelId};

use crate::{
    bytes::FileBytes,
    nums::{errno, sys},
    ring,
};

/// A file the guest opened, held entirely in supervisor memory.
///
/// Content, length and position are the model's own, never the host's.
#[derive(Clone, Copy, Debug, Default)]
struct OpenFile {
    /// The content this descriptor refers to. Absent for the endless random source.
    ///
    /// A number, since a descriptor outlives an unlinked name.
    inode: Option<u64>,
    /// Where the next positional-free read or write lands.
    position: u64,
    /// Whether a write through this descriptor is durable when it returns.
    ///
    /// Set by `O_DSYNC` and `O_SYNC` at open, which careful stores use instead of `fsync`.
    durable: bool,
    /// Whether every write through this descriptor lands at the end of the file.
    ///
    /// Set by `O_APPEND` at open and by `F_SETFL`.
    appending: bool,
    /// The flags `F_GETFL` reports: the access mode, `O_APPEND`, `O_NONBLOCK` and the rest, as
    /// the open gave them and `F_SETFL` changed them.
    status: u64,
}

/// Size of the `struct sysinfo` a caller reads back.
///
/// Written as zeros, since every field is host state.
const SYSINFO_BYTES: usize = 112;

/// The default a `stat` reports for anything this model offers.
mod stat {
    /// Device the guest sees every file on.
    pub(crate) const DEVICE: u64 = 1;
    /// Block size reported for every file, which callers size their buffers from.
    pub(crate) const BLOCK_SIZE: u64 = 4096;
    /// `S_IFREG | 0644`.
    pub(crate) const REGULAR_MODE: u32 = 0o100_644;
    /// `S_IFCHR | 0666`, which is what a random source is.
    pub(crate) const CHAR_MODE: u32 = 0o020_666;
    /// `S_IFDIR | 0755`.
    pub(crate) const DIRECTORY_MODE: u32 = 0o040_755;
}

/// The `fcntl` commands this model tells apart.
///
/// Only the two that make a descriptor; the rest answer zero.
mod fcntl_cmd {
    /// Lowest free descriptor at or above the third argument, onto the same open file.
    pub(crate) const DUPFD: u32 = 0;
    /// The descriptor's own flags, `FD_CLOEXEC`.
    pub(crate) const GETFD: u32 = 1;
    pub(crate) const SETFD: u32 = 2;
    /// The open file's status flags.
    pub(crate) const GETFL: u32 = 3;
    pub(crate) const SETFL: u32 = 4;
    /// Record locks, process-associated and open-file-associated.
    pub(crate) const GETLK: u32 = 5;
    pub(crate) const SETLK: u32 = 6;
    pub(crate) const SETLKW: u32 = 7;
    pub(crate) const OFD_GETLK: u32 = 36;
    pub(crate) const OFD_SETLK: u32 = 37;
    pub(crate) const OFD_SETLKW: u32 = 38;
    /// Signal ownership: `F_SETOWN`, `F_GETOWN`, `F_SETSIG`, `F_GETSIG`, `F_SETOWN_EX`,
    /// `F_GETOWN_EX`. No signal is ever sent for I/O here.
    pub(crate) const OWNERSHIP: [u32; 6] = [8, 9, 10, 11, 15, 16];
    /// Leases and directory notices: `F_SETLEASE`, `F_NOTIFY`; and `F_GETLEASE`, which reads none.
    pub(crate) const SETLEASE: u32 = 1024;
    pub(crate) const GETLEASE: u32 = 1025;
    pub(crate) const NOTIFY: u32 = 1026;
    /// The same as `DUPFD`, with the copy marked `FD_CLOEXEC`.
    pub(crate) const DUPFD_CLOEXEC: u32 = 1030;
    /// Write-life hints, `F_GET_RW_HINT` and `F_SET_RW_HINT`, which change nothing here.
    pub(crate) const RW_HINTS: [u32; 2] = [1035, 1036];
    /// `FD_CLOEXEC`, the one descriptor flag.
    pub(crate) const FD_CLOEXEC: u64 = 1;
    /// `F_UNLCK`, the lock type `F_GETLK` writes where nothing stands in the way.
    pub(crate) const UNLOCKED: i16 = 2;
}

/// The `futex` operations a threaded guest reaches, with the flags glibc adds masked off.
pub(crate) mod futex_op {
    /// Block while the word still holds the expected value.
    pub(crate) const WAIT: u32 = 0;
    /// Release up to `val` waiters.
    pub(crate) const WAKE: u32 = 1;
    /// Move waiters to a second word, releasing `val` of them first.
    pub(crate) const REQUEUE: u32 = 3;
    /// `REQUEUE`, conditional on the first word still holding the expected value.
    pub(crate) const CMP_REQUEUE: u32 = 4;
    /// `WAIT` with an absolute deadline and a mask.
    pub(crate) const WAIT_BITSET: u32 = 9;
    /// `WAKE` restricted to waiters whose mask overlaps.
    pub(crate) const WAKE_BITSET: u32 = 10;
    /// Take a lock whose word carries the identifier of the thread holding it.
    pub(crate) const LOCK_PI: u32 = 6;
    /// Release one, handing it to whoever is queued.
    pub(crate) const UNLOCK_PI: u32 = 7;
    /// `LOCK_PI` that returns at once.
    pub(crate) const TRYLOCK_PI: u32 = 8;

    /// The bit a held word carries while somebody is queued behind it.
    pub(crate) const WAITERS: u32 = 0x8000_0000;

    /// Set when the word is private to one process, which changes nothing this model does.
    pub(crate) const PRIVATE_FLAG: u32 = 128;
    /// Set when an absolute timeout is a wall-clock time.
    pub(crate) const CLOCK_REALTIME: u32 = 256;

    /// The operation, with the flags that do not change it removed.
    pub(crate) const fn base(op: u32) -> u32 {
        op & !(PRIVATE_FLAG | CLOCK_REALTIME)
    }
}

/// Descriptors the guest starts with. Anything above is handed out by `openat`.
const FIRST_FREE_FD: i64 = 3;

/// A window onto a file, held in the guest's address space.
///
/// A read-only mapping is refreshed from the file whenever the file changes.
///
/// A writable mapping holds the current contents. The world copies them back when the file is read
/// through a descriptor and when it is synced, never at a crash.
#[derive(Clone, Copy, Debug)]
struct Mapping {
    /// Where it landed in the guest.
    at: u64,
    /// How many bytes of it there are.
    len: u64,
    /// The file it shows.
    inode: u64,
    /// Where in that file it starts.
    offset: u64,
    /// Whether the guest may write through it, which is what decides the direction above.
    writable: bool,
    /// The descriptor it was made from, kept for narration only.
    ///
    /// A label only; the mapping outlives the descriptor and works on the inode.
    fd: i64,
}

/// A mapping asked for and not yet placed: the call has been replaced and has not returned.
#[derive(Clone, Copy, Debug)]
struct PendingMap {
    len: u64,
    inode: u64,
    offset: u64,
    writable: bool,
    fd: i64,
    /// Which region of a submission ring this is, where it is one.
    ring: Option<u64>,
}

/// What `io_uring_enter` took from a ring's queue, to be carried out by whichever world routes it.
pub(crate) struct Taken {
    /// The ring's descriptor, where the completions go.
    pub(crate) fd: i64,
    /// The entries, in the order the guest queued them.
    pub(crate) submissions: Vec<ring::Submission>,
}

/// The run's end for a submission this ring does not carry, named by what it asked for.
pub(crate) fn refuse_submission(ctx: &mut StopCtx<'_, '_>, what: &str) -> KernelError {
    ctx.refuse(
        "BOHR-2004",
        format_args!("io_uring {what} is not modelled; see docs/PLATFORMS.md §4.1"),
    )
}

/// What a submission returned, read off the effects of the plain call that carried it out.
///
/// A submission that would park or reach the host is refused, since every completion is posted
/// before `io_uring_enter` returns.
pub(crate) fn submission_answer(
    ctx: &mut StopCtx<'_, '_>,
    effects: &Effects,
) -> Result<i64, KernelError> {
    match effects.state {
        bohr_kernel::TaskState::Runnable(
            Resume::Emulate { ret } | Resume::EmulateAndSignal { ret, .. },
        ) => Ok(ret),
        _ => Err(refuse_submission(ctx, "a submission that waits or reaches the host")),
    }
}

/// The epoch every wall clock starts from, in whole seconds, as a file's times are stated.
fn epoch_seconds() -> i64 {
    (bohr_kernel::WALL_EPOCH.get() / 1_000_000_000) as i64
}

/// The one descriptor number this model never hands out.
///
/// The coverage channel's descriptor (`docs/API.md` §5.5), chosen by the interceptor and skipped
/// here.
const RESERVED_FD: i64 = 200;

/// Longest path this model resolves; longer names are refused, never truncated.
const PATH_MAX: usize = 4096;

/// Largest transfer per call; larger requests get a short read.
const MAX_TRANSFER: usize = 1 << 20;

/// Virtual time a syscall costs, uniform without measurements to justify more.
pub(crate) const SYSCALL_COST: Tick = Tick(1_000);

/// Identity the guest reports for itself.
const GUEST_PID: i64 = 1;

/// Offset of `child_tid` within the `clone_args` structure `clone3` takes by pointer.
const CLONE_ARGS_CHILD_TID: u64 = 16;

/// Open flags this model reads. The rest describe access the model does not distinguish.
const O_CREAT: u64 = 0o100;
/// Truncate on open.
const O_TRUNC: u64 = 0o1000;
/// The name must be a directory, and the open fails where it is not.
const O_DIRECTORY: u64 = 0o200_000;
/// `O_DSYNC`, and `O_SYNC` which contains it: a write through this descriptor is durable when it
/// returns, so the caller never issues a sync of its own.
const O_WRITE_THROUGH: u64 = 0o10_000;
/// The two access modes that would write; a directory refuses both.
const O_WRITABLE: u64 = 0o3;
/// Every write goes to the end of the file, whatever the descriptor's position says.
const O_APPEND: u64 = 0o2000;
/// The open must create the file, and fails where the name is taken.
const O_EXCL: u64 = 0o200;

/// The flags `F_GETFL` reports and `F_SETFL` may change, as Linux keeps them on an open file.
mod status {
    /// The access mode, `O_RDONLY`, `O_WRONLY` or `O_RDWR`.
    pub(crate) const ACCESS: u64 = 0o3;
    /// `O_RDWR`, which the standard descriptors and the model's own handles report.
    pub(crate) const READ_WRITE: u64 = 0o2;
    /// `O_NONBLOCK`.
    pub(crate) const NONBLOCK: u64 = 0o4000;
    /// `O_LARGEFILE`, which a 64-bit kernel sets on every open.
    pub(crate) const LARGEFILE: u64 = 0o100_000;
    /// `O_ASYNC`, `O_DIRECT` and `O_NOATIME`, which `F_SETFL` changes and the model only keeps.
    pub(crate) const KEPT_ONLY: u64 = 0o20_000 | 0o40_000 | 0o1_000_000;
    /// `O_DSYNC` and `__O_SYNC`, which make up `O_SYNC`.
    pub(crate) const SYNC: u64 = 0o10_000 | 0o4_000_000;
    /// What an open records: the access mode and every flag `F_GETFL` reports.
    pub(crate) const OPENED: u64 = ACCESS | super::O_APPEND | NONBLOCK | KEPT_ONLY | SYNC;
    /// What `F_SETFL` may change; the access mode and the sync flags it leaves alone.
    pub(crate) const SETTABLE: u64 = super::O_APPEND | NONBLOCK | KEPT_ONLY;
}

/// `O_CLOEXEC`: the descriptor is closed on exec, a mark on its number.
const O_CLOEXEC: u64 = 0o2_000_000;

/// The directory descriptor an `*at` call carries, read at the width the kernel reads it.
///
/// As a 32-bit `int`, so `AT_FDCWD` reads as -100.
fn dirfd_of(raw: u64) -> i64 {
    i64::from(raw as i32)
}

/// `AT_FDCWD`: the call names its own working directory.
const AT_FDCWD: i64 = -100;

/// `AT_EMPTY_PATH`: the call is about the descriptor itself.
const AT_EMPTY_PATH: u64 = 0x1000;

/// The one directory that exists before a guest makes any, and the only path with no parent.
const ROOT: &str = "/";
/// Its number. Guest files are numbered after it and after the directories below.
const ROOT_INODE: u64 = 1;

/// What a guest is told when it asks the kernel about itself.
///
/// Generated, fixed and small; every number is the model's own.
///
/// Only files real targets read. Anything else under `/proc` is absent.
fn generated(path: &str) -> Option<Vec<u8>> {
    // Field 22 is the start time, which readers want; all fields are zero.
    let stat = format!(
        "{GUEST_PID} (guest) S 0 {GUEST_PID} {GUEST_PID} 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 \
         100 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n"
    );
    let own = format!("/proc/{GUEST_PID}/stat");
    match path {
        "/proc/self/stat" => Some(stat.into_bytes()),
        p if p == own => Some(stat.into_bytes()),
        // Uptime and idle time, fixed.
        "/proc/uptime" => Some(b"100.00 100.00\n".to_vec()),
        _ => None,
    }
}

/// The directories a guest may assume are already there.
///
/// Every real filesystem has these, so a program may write to `/tmp` without creating it.
///
/// From the hierarchy standard, parents first, in a fixed order that sets their numbering. Empty
/// until a guest writes.
const STANDARD_DIRECTORIES: [&str; 16] = [
    "/bin", "/dev", "/etc", "/home", "/lib", "/opt", "/proc", "/root", "/run", "/sbin", "/srv",
    "/sys", "/tmp", "/usr", "/var", "/var/tmp",
];

/// `lseek` origins.
const SEEK_SET: u64 = 0;
/// From the current position.
const SEEK_CUR: u64 = 1;
/// From the end.
const SEEK_END: u64 = 2;

/// `clock_nanosleep` flag asking for an absolute deadline.
pub(crate) const TIMER_ABSTIME: u64 = 1;

/// Signals that end a task.
const FATAL_SIGNALS: [u32; 5] = [4, 6, 8, 9, 11];

/// Size of `struct pollfd`.
const POLLFD_BYTES: usize = 8;

/// Most descriptors a single `poll` is answered for.
const MAX_POLL_ENTRIES: usize = 1024;

/// Size of one `utsname` field, including the terminator.
const UTSNAME_FIELD: usize = 65;

/// Size of `struct utsname`: six fields of the size above.
const UTSNAME_BYTES: usize = UTSNAME_FIELD * 6;

/// Kernel release shown to the guest, fixed since guests branch on it.
const PRESENTED_RELEASE: &str = "5.15.0-bohr";

/// Kernel version string, fixed.
const PRESENTED_VERSION: &str = "#1 SMP bohr";

/// Size of `struct stat` on x86-64.
const STAT_BYTES: usize = 144;

/// Size of `struct statx`, which is the same on every architecture.
const STATX_BYTES: usize = 256;

/// Size of `struct rusage` on x86-64: two intervals and fourteen counters.
const RUSAGE_BYTES: usize = 144;

/// Bytes of a `struct rlimit`: the soft limit and the hard one.
const RLIMIT_BYTES: usize = 16;

/// `RLIMIT_NOFILE`, the one a runtime actually reads.
const RLIMIT_NOFILE: u64 = 7;

/// What this machine says its limits are, and they are the same machine everywhere.
///
/// Descriptors get stock Linux's limit; everything else is unlimited.
const fn limit_for(resource: u64) -> (u64, u64) {
    match resource {
        RLIMIT_NOFILE => (1024, 1_048_576),
        // The limit the backend starts every guest under.
        RLIMIT_STACK => (bohr_backend::GUEST_STACK_BYTES, u64::MAX),
        _ => (u64::MAX, u64::MAX),
    }
}

/// `RLIMIT_STACK`.
const RLIMIT_STACK: u64 = 3;

/// Size of `struct statfs` on x86-64.
const STATFS_BYTES: usize = 120;

/// Bytes of `struct linux_dirent64` before its name: an inode, an offset, a length and a kind.
const DIRENT_HEADER_BYTES: usize = 19;

/// What kind of thing a directory entry names.
mod dirent {
    /// `DT_DIR`.
    pub(crate) const DIRECTORY: u8 = 4;
    /// `DT_REG`.
    pub(crate) const REGULAR: u8 = 8;
}

/// What the filesystem this model stands for reports about itself.
mod filesystem {
    /// `EXT4_SUPER_MAGIC`, since the disk model follows ext4's `data=ordered`.
    pub(crate) const MAGIC: u64 = 0xEF53;
    /// Blocks of [`super::stat::BLOCK_SIZE`]: 16 GiB, never the binding limit.
    pub(crate) const BLOCKS: u64 = 4 * 1024 * 1024;
    /// Longest name component, which is every real Linux filesystem's answer.
    pub(crate) const NAME_MAX: u64 = 255;
}

/// The first file's inode number, fixed.
const FIRST_INODE: u64 = 2;

/// Writes `value` into `buf` at `offset`, for building a structure the guest reads.
fn put(buf: &mut [u8], offset: usize, value: &[u8]) {
    buf[offset..offset + value.len()].copy_from_slice(value);
}

/// The directory a name sits in, or nothing where the name is the root itself.
fn parent_of(path: &str) -> Option<&str> {
    if path == ROOT {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some(ROOT),
        Some(cut) => Some(&path[..cut]),
        None => None,
    }
}

/// Size of one `iovec`: a base pointer and a length.
const IOVEC_BYTES: usize = 16;

/// Most segments a gathering write is answered for.
const MAX_IOVECS: u64 = 1024;

/// Whether a vectored call names more pieces than one may carry.
///
/// Checked first; Linux answers `EINVAL` for the whole call.
pub(crate) const fn too_many_iovecs(count: u64) -> bool {
    count > MAX_IOVECS
}

/// The largest a stored file may become.
///
/// Files live in supervisor memory, so a far seek and write is refused.
const MAX_FILE_BYTES: usize = 1 << 26;

/// Where a write ends, or nothing where it would take the file past what may be held.
fn grown_end(at: usize, len: usize) -> Option<usize> {
    at.checked_add(len).filter(|end| *end <= MAX_FILE_BYTES)
}

/// A guest address at an offset from another, or nothing where the sum leaves the address space.
///
/// Every walk over a guest array goes through here, so wrapping is refused.
fn offset(addr: GuestAddr, by: u64) -> Option<GuestAddr> {
    addr.0.checked_add(by).map(GuestAddr)
}

/// The single file this model offers, and the only path that resolves.
const RANDOM_PATHS: [&str; 2] = ["/dev/urandom", "/dev/random"];

/// One task waiting on one futex word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Waiter {
    task: TaskId,
    /// Mask a `WAKE_BITSET` must overlap to release this waiter. All ones for a plain wait.
    bits: u32,
}

/// A whole disk at one moment: what a crash model snapshots at `fsync` and restores at a kill.
///
/// File bytes are shared by block, so an image costs a reference per block.
#[derive(Clone, Debug)]
pub(crate) struct DiskImage {
    pub(crate) content: BTreeMap<u64, FileBytes>,
    pub(crate) names: BTreeMap<String, u64>,
    /// Symbolic links and the text each holds; names on the disk like any other, synced with
    /// the namespace.
    pub(crate) links: BTreeMap<String, String>,
    /// Which of those names are directories, restored together with them.
    pub(crate) directories: BTreeSet<u64>,
    pub(crate) next_inode: u64,
}

/// Where a guest's own output is delivered.
///
/// Supplied by the caller, so this crate does no host I/O.
pub type OutputSink = Box<dyn std::io::Write + Send>;

/// The `clone` flags the world reads.
mod clone_flags {
    /// `CLONE_UNTRACED`: the new task is not traced, whatever the tracer asked.
    pub(super) const UNTRACED: u64 = 0x0080_0000;
    /// `CLONE_NEWNS`, `CLONE_NEWCGROUP`, `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWUSER`,
    /// `CLONE_NEWPID`, `CLONE_NEWNET` and `CLONE_NEWTIME`.
    pub(super) const NEW_NAMESPACES: u64 = 0x0002_0000
        | 0x0200_0000
        | 0x0400_0000
        | 0x0800_0000
        | 0x1000_0000
        | 0x2000_0000
        | 0x4000_0000
        | 0x80;
}

/// What a subsystem answers a call with.
pub(crate) enum Answer {
    /// The return value, how the guest resumes, and the digest of the data it wrote, where it
    /// wrote any; the world records the call.
    Reply((i64, Resume, Option<[u8; 8]>)),
    /// Effects the subsystem built and recorded itself, such as a wait.
    Done(Effects),
}

/// The parts of the world a call is routed to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Subsystem {
    Vfs,
    Clock,
    Process,
    Signals,
    IoUring,
    Mappings,
}

/// Which subsystem answers which calls; a call in none is unmodelled. `futex` and task creation
/// build their own effects before routing, and are listed for what the world answers.
const ROUTES: [(Subsystem, &[u32]); 6] = [
    (Subsystem::Vfs, vfs::HANDLES),
    (Subsystem::Clock, clock::HANDLES),
    (Subsystem::Process, process::HANDLES),
    (Subsystem::Signals, signals::HANDLES),
    (Subsystem::IoUring, io_uring::HANDLES),
    (Subsystem::Mappings, mappings::HANDLES),
];

/// The calls answered before routing: waiting on a word, and creating a task.
#[cfg(test)]
const ANSWERED_FIRST: [u32; 3] = [sys::FUTEX, sys::CLONE, sys::CLONE3];

/// The subsystem that answers `nr`, where one does.
fn route(nr: u32) -> Option<Subsystem> {
    ROUTES.iter().find(|(_, handles)| handles.contains(&nr)).map(|(subsystem, _)| *subsystem)
}

/// Every call the minimal world answers.
#[cfg(test)]
pub(crate) fn answered() -> impl Iterator<Item = u32> {
    ROUTES.iter().flat_map(|(_, handles)| handles.iter().copied()).chain(ANSWERED_FIRST)
}

/// Answers clock, randomness and output, and refuses everything else.
pub struct MinimalWorld {
    /// Synchronisation words this run has touched, in the order it first touched them.
    ///
    /// Kept so a trace can name a word without naming its address; see [`Self::sync_word`].
    sync_words: BTreeMap<GuestAddr, SyncWordId>,
    output: Option<OutputSink>,
    /// Descriptors the guest holds, and what each refers to.
    open: BTreeMap<i64, OpenFile>,
    /// File descriptors marked `FD_CLOEXEC`; the mark belongs to the number, not to the file.
    cloexec: BTreeSet<i64>,
    /// Space `fallocate` reserved past a file's end under `FALLOC_FL_KEEP_SIZE`: the end of the
    /// reservation, by inode. A truncate below it gives it back.
    reserved: BTreeMap<u64, u64>,
    /// Numbers held by descriptors kept outside this world: sockets, pollers, counters.
    ///
    /// One process has one table of numbers, so a file never takes a number a socket holds.
    foreign: BTreeSet<i64>,
    /// File contents by inode, never on the host. Shared by block with disk images.
    content: BTreeMap<u64, FileBytes>,
    /// Names, each pointing at content; unlinking keeps the content for open descriptors.
    names: BTreeMap<String, u64>,
    /// Files this world generates itself, off every disk.
    ///
    /// Read like files but kept out of every disk image, so crashes never affect them.
    synthetic: BTreeSet<u64>,
    /// Live mappings of files, by where each landed.
    mappings: BTreeMap<u64, Mapping>,
    /// What each task's replaced `mmap` was for, until it returns and says where it went.
    mapping: BTreeMap<TaskId, PendingMap>,
    /// Which of those names are directories.
    ///
    /// A directory is a name that can be made, queried and opened. Paths resolve whole, without
    /// walking components.
    directories: BTreeSet<u64>,
    /// Whether the guest is shown a kernel with `io_uring` (`[linux] io_uring = "ring"`).
    io_uring: bool,
    /// Permission bits a guest set with the `chmod` family, by inode; the rest read as made.
    ///
    /// Kept apart from content, so a restarted node sees the original bits.
    modes: BTreeMap<u64, u32>,
    /// What each task called itself with `PR_SET_NAME`, sixteen bytes as the kernel keeps it.
    thread_names: BTreeMap<TaskId, [u8; 16]>,
    /// Timed futex waits this world arms itself, when no cluster arms them for it: by deadline,
    /// then by task, each with the word the task waits on.
    timeouts: BTreeMap<(Tick, TaskId), (u64, u64)>,
    /// Each task's futex waits, counted, so a timer armed for one wait cannot end a later one.
    wait_seq: BTreeMap<TaskId, u64>,
    /// Submission rings the guest has open, by the descriptor each was given.
    rings: BTreeMap<i64, ring::Ring>,
    /// Whether the guest has already been told this kernel has no `io_uring`.
    ///
    /// Once per run, so a probing loop does not flood the timeline.
    declined_ring: bool,
    /// The directory behind each descriptor a guest opened one through.
    ///
    /// Kept apart, since only directory calls ask.
    dir_of: BTreeMap<i64, String>,
    /// The program this world was started with, as `/proc/self/exe` reports it.
    ///
    /// The run's argument, so a replay gives the same answer.
    program: Option<String>,
    /// Names that stand for other names, and the text each was created with.
    ///
    /// The text, as `readlink` answers; a relative target resolves when followed.
    links: BTreeMap<String, String>,
    /// The number the next file created is given.
    next_inode: u64,
    /// Tasks waiting on each futex word, keyed by guest address.
    ///
    /// The model's own queue, so wake order never comes from the host.
    waiters: BTreeMap<u64, Vec<Waiter>>,
    /// The word each task clears when it ends, read from the call that created it.
    exit_word: BTreeMap<TaskId, u64>,
    /// The word named by the most recent creation call in each task, waiting for the task it
    /// produced to be announced.
    creating: BTreeMap<TaskId, u64>,
    /// Signals sent to a task by another, waiting for that task's next stop.
    pending_signals: BTreeMap<TaskId, Vec<u32>>,
    /// Signals this world has had the kernel raise on a task, which come back as a stop to be let
    /// through.
    raised: BTreeMap<TaskId, Vec<u32>>,
    /// Tasks this world has seen begin, and those it has seen end, to tell a thread from a number.
    born: BTreeSet<TaskId>,
    ended: BTreeSet<TaskId>,
    /// Reused across transfers so a guest reading in a loop does not allocate per call.
    transfer: Vec<u8>,
    /// Everything the guest wrote to its standard output, where a projection asks for it.
    ///
    /// Kept only when a projection will read it.
    stdout: Option<Vec<u8>>,
    /// The end of what the guest wrote to standard output and error, for a bundle to keep.
    ///
    /// Kept only for a run whose log is kept; a cluster keeps its own for every node.
    tails: Option<bohr_trace::OutputTails>,
    /// What `getcwd` answers.
    ///
    /// Part of the filesystem state.
    cwd: String,
    /// Whether something outside this world arms the timeout a wait carries.
    timeouts_armed: bool,
    /// The calls this build cannot model, and how often each was asked for.
    ///
    /// `Some` only under a survey; a run refuses the first (§5.7).
    surveying: Option<BTreeMap<u32, u64>>,
}

impl std::fmt::Debug for MinimalWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinimalWorld")
            .field("open", &self.open)
            .field("output", &self.output.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for MinimalWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl MinimalWorld {
    /// A world offering no faults, whose guest output goes nowhere.
    #[must_use]
    pub fn new() -> Self {
        let mut world = Self::bare();
        for path in STANDARD_DIRECTORIES {
            world.add_directory(path);
        }
        // The guest's `/proc` entry exists from the start, so directory listings find it.
        world.add_directory(&format!("/proc/{GUEST_PID}"));
        world
    }

    /// The world with only the root, before the standard directories are added.
    fn bare() -> Self {
        Self {
            output: None,
            sync_words: BTreeMap::new(),
            // The three standard descriptors are open, as a Go runtime checks before it starts.
            open: (0..=2)
                .map(|fd| {
                    (
                        fd,
                        OpenFile {
                            status: status::READ_WRITE | status::LARGEFILE,
                            ..OpenFile::default()
                        },
                    )
                })
                .collect(),
            cloexec: BTreeSet::new(),
            reserved: BTreeMap::new(),
            foreign: BTreeSet::new(),
            content: BTreeMap::new(),
            names: [(ROOT.to_owned(), ROOT_INODE)].into_iter().collect(),
            synthetic: BTreeSet::new(),
            mappings: BTreeMap::new(),
            mapping: BTreeMap::new(),
            directories: [ROOT_INODE].into_iter().collect(),
            io_uring: false,
            modes: BTreeMap::new(),
            thread_names: BTreeMap::new(),
            timeouts: BTreeMap::new(),
            wait_seq: BTreeMap::new(),
            rings: BTreeMap::new(),
            declined_ring: false,
            dir_of: BTreeMap::new(),
            program: None,
            links: BTreeMap::new(),
            next_inode: FIRST_INODE,
            waiters: BTreeMap::new(),
            exit_word: BTreeMap::new(),
            creating: BTreeMap::new(),
            pending_signals: BTreeMap::new(),
            raised: BTreeMap::new(),
            born: BTreeSet::new(),
            ended: BTreeSet::new(),
            transfer: Vec::new(),
            stdout: None,
            tails: None,
            cwd: "/".to_owned(),
            surveying: None,
            timeouts_armed: false,
        }
    }

    /// The same world, with somebody outside it arming the timeouts a wait can carry.
    ///
    /// Set by the cluster for its nodes, which arms `FutexTimeout`. A world alone arms its own.
    #[must_use]
    pub fn with_timeouts_armed(mut self) -> Self {
        self.timeouts_armed = true;
        self
    }

    /// The same world, keeping what the guest writes out so a projection can read it back.
    #[must_use]
    pub fn keeping_output(mut self) -> Self {
        self.stdout = Some(Vec::new());
        self
    }

    /// The same world, keeping the end of what the guest writes out for its bundle.
    #[must_use]
    pub fn keeping_tails(mut self) -> Self {
        self.tails = Some(bohr_trace::OutputTails::new());
        self
    }

    /// The end of what the guest wrote out, where the world kept it.
    #[must_use]
    pub fn output_tails(&self) -> Vec<bohr_trace::OutputTail> {
        self.tails.as_ref().map(bohr_trace::OutputTails::records).unwrap_or_default()
    }

    /// Adds bytes the guest wrote out to what a projection will read.
    ///
    /// Public, since the cluster intercepts standard output and hands it back here (§5.20).
    pub fn keep_written_out(&mut self, bytes: &[u8]) {
        if let Some(kept) = self.stdout.as_mut() {
            let room = crate::MAX_PROJECTION_BYTES.saturating_sub(kept.len());
            kept.extend_from_slice(&bytes[..room.min(bytes.len())]);
        }
    }

    /// What one projection came to on this world.
    #[must_use]
    pub fn project(&self, source: &crate::Source) -> Option<crate::Value> {
        let bytes = match source {
            crate::Source::File(path) => {
                let inode = self.names.get(path)?;
                self.content.get(inode)?.to_vec()
            }
            crate::Source::Stdout => self.stdout.clone()?,
        };
        let truncated = bytes.len() >= crate::MAX_PROJECTION_BYTES;
        Some(crate::Value { bytes, truncated })
    }

    /// The same world, with the working directory a guest is told it is in.
    #[must_use]
    pub fn with_cwd(mut self, cwd: String) -> Self {
        self.cwd = cwd;
        self
    }

    /// The same world, with a filesystem already on it.
    ///
    /// Applied before anything runs, so the durability baseline is the prepared state.
    #[must_use]
    pub fn from_state(mut self, image: &crate::StateImage) -> Self {
        for (path, content) in image.files() {
            // Parent directories first, so `stat` on them succeeds.
            self.make_parents(path);
            let inode = self.next_inode;
            self.next_inode += 1;
            // Shared with the image, block by block, until the guest writes.
            self.content.insert(inode, content.clone());
            self.names.insert(path.clone(), inode);
        }
        self
    }

    /// The same world, delivering what the guest writes to `sink`.
    #[must_use]
    pub fn with_output(mut self, sink: OutputSink) -> Self {
        self.output = Some(sink);
        self
    }

    /// The same world, showing the guest a kernel that has `io_uring` and `statx`.
    ///
    /// Only a cluster enables this, since it routes ring entries.
    #[must_use]
    pub(crate) fn with_io_uring(mut self) -> Self {
        self.io_uring = true;
        self
    }

    /// The same world, told which program it is running, for the calls that ask.
    #[must_use]
    pub fn with_program(mut self, program: String) -> Self {
        self.program = Some(program);
        self
    }

    /// The same world, answering a call it cannot model instead of refusing it, and writing down
    /// what it answered.
    ///
    /// Its identity changes, so `Manifest::check_provenance` refuses survey bundles.
    #[must_use]
    pub fn surveying(mut self) -> Self {
        self.surveying = Some(BTreeMap::new());
        self
    }

    /// What a survey found it could not model, by syscall number.
    #[must_use]
    pub fn surveyed(&self) -> Vec<(u32, u64)> {
        self.surveying
            .as_ref()
            .map(|seen| seen.iter().map(|(n, c)| (*n, *c)).collect())
            .unwrap_or_default()
    }

    /// What to do about a syscall no model here answers.
    ///
    /// A run refuses (§5.7); a survey answers `ENOSYS` and records it.
    ///
    /// Both refusals live here; io_uring has its own, since there is no table row to add.
    fn unmodelled(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        nr: u32,
        args: [u64; 6],
    ) -> Result<i64, KernelError> {
        let ring = sys::IO_URING.contains(&nr);
        let Some(seen) = self.surveying.as_mut() else {
            return Err(if ring {
                ctx.refuse("BOHR-2004", "io_uring is not modelled; see docs/PLATFORMS.md §4.1")
            } else if nr == sys::MMAP {
                ctx.refuse(
                    "BOHR-2001",
                    format_args!(
                        "a mapping of a file is not modelled: {}; see docs/PLATFORMS.md §4.3",
                        self.describe_mapping(args)
                    ),
                )
            } else {
                ctx.refuse(
                    "BOHR-2001",
                    format_args!("unsupported syscall {nr}; see docs/PLATFORMS.md §4.2"),
                )
            });
        };

        let count = seen.entry(nr).or_default();
        *count += 1;
        // Once per number.
        if *count == 1 {
            let code = if ring { "BOHR-2004" } else { "BOHR-2001" };
            ctx.emit(EventKind::Diagnostic {
                code: code.to_owned(),
                message: format!("syscall {nr} answered ENOSYS by a survey rather than modelled"),
            })?;
        }
        Ok(-errno::ENOSYS)
    }

    /// Opens a submission ring, a polled one included.
    ///
    /// A polled queue starts with its poller asleep, so callers wake it with `io_uring_enter`.
    fn open_ring(&mut self, ctx: &mut StopCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
        let mut params = [0u8; 120];
        ctx.read_mem(GuestAddr(args[1]), &mut params)?;
        let word = |at: usize| {
            u32::from_le_bytes([params[at], params[at + 1], params[at + 2], params[at + 3]])
        };
        let held = match ring::Ring::from_setup(args[0] as u32, word(8), word(4)) {
            Ok(held) => held,
            Err(ring::Setup::Invalid) => return Ok(-errno::EINVAL),
            Err(ring::Setup::Unsupported(flag)) => return Err(refuse_submission(ctx, flag)),
        };
        ctx.write_mem(GuestAddr(args[1]), &held.params())?;
        let fd = self.attach(OpenFile {
            status: status::READ_WRITE | status::LARGEFILE,
            ..OpenFile::default()
        });
        self.rings.insert(fd, held);
        ctx.emit(EventKind::Diagnostic {
            code: "BOHR-2004".to_owned(),
            message: format!("io_uring of {} entries opened as descriptor {fd}", held.entries),
        })?;
        Ok(fd)
    }

    /// Whether a descriptor is a ring this world opened.
    pub(crate) fn holds_ring(&self, fd: i64) -> bool {
        self.rings.contains_key(&fd)
    }

    /// Takes what the guest has queued for `io_uring_enter`, or the errno the call answers instead.
    ///
    /// No more entries than free completion slots, so `IORING_FEAT_NODROP` holds; a full queue is
    /// `EBUSY`. A bad index is dropped and ends the batch, as in a kernel.
    pub(crate) fn take_submissions(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        args: [u64; 6],
    ) -> Result<Result<Taken, i64>, KernelError> {
        let fd = args[0] as i64;
        let Some(held) = self.rings.get(&fd).copied() else {
            return Ok(Err(errno::EBADF));
        };
        if !held.mapped() {
            return Ok(Err(errno::EINVAL));
        }
        let head = Self::read_u32(ctx, held.sq_head_at())?;
        let tail = Self::read_u32(ctx, held.sq_tail_at())?;
        let reaped = Self::read_u32(ctx, held.cq_head_at())?;
        let posted = Self::read_u32(ctx, held.cq_tail_at())?;
        let free = held.completions().saturating_sub(posted.wrapping_sub(reaped));
        // A polled queue drains whole; liburing passes a count of 0.
        let asked = if held.polled { u32::MAX } else { args[1] as u32 };
        let wanted = tail.wrapping_sub(head).min(held.entries).min(asked);
        if wanted > 0 && free == 0 {
            return Ok(Err(errno::EBUSY));
        }
        let taking = wanted.min(free);

        let mut submissions = Vec::with_capacity(taking as usize);
        let mut dropped = 0u32;
        for position in 0..taking {
            let index = Self::read_u32(ctx, held.sq_array_at(head.wrapping_add(position)))?;
            let Some(at) = held.sqe_at(index) else {
                dropped = 1;
                break;
            };
            let mut bytes = [0u8; ring::SQE_BYTES as usize];
            ctx.read_mem(GuestAddr(at), &mut bytes)?;
            submissions.push(ring::Submission::read(&bytes));
        }
        // The dropped entry is consumed with the ones before it; the ones after it wait.
        let consumed = submissions.len() as u32 + dropped;
        if let Some(at) = held.sq_head_at() {
            ctx.write_mem(GuestAddr(at), &head.wrapping_add(consumed).to_le_bytes())?;
        }
        if dropped > 0
            && let Some(at) = held.sq_dropped_at()
        {
            let before = Self::read_u32(ctx, Some(at))?;
            ctx.write_mem(GuestAddr(at), &before.wrapping_add(dropped).to_le_bytes())?;
        }
        Ok(Ok(Taken { fd, submissions }))
    }

    /// Posts completions to a ring's queue and moves its tail past them.
    pub(crate) fn post_completions(
        &self,
        ctx: &mut StopCtx<'_, '_>,
        fd: i64,
        done: &[ring::Completed],
    ) -> Result<(), KernelError> {
        // A submission that closed the ring's own descriptor leaves nowhere to post.
        let Some(held) = self.rings.get(&fd).copied() else {
            return Ok(());
        };
        let mut tail = Self::read_u32(ctx, held.cq_tail_at())?;
        for (user_data, result) in done {
            if let Some(at) = held.cqe_at(tail) {
                ctx.write_mem(GuestAddr(at), &ring::completion(*user_data, *result))?;
            }
            tail = tail.wrapping_add(1);
        }
        if let Some(at) = held.cq_tail_at() {
            ctx.write_mem(GuestAddr(at), &tail.to_le_bytes())?;
        }
        Ok(())
    }

    /// Reads one of the counters the two queues are steered by.
    fn read_u32(ctx: &mut StopCtx<'_, '_>, at: Option<u64>) -> Result<u32, KernelError> {
        let Some(at) = at else {
            return Ok(0);
        };
        let mut bytes = [0u8; 4];
        ctx.read_mem(GuestAddr(at), &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Answers the call that opens a ring, and says so on the timeline.
    ///
    /// Declined with `ENOSYS`, as a kernel without io_uring does, so the guest falls back to plain
    /// calls. Entering or registering a ring stays refused.
    ///
    /// The fallback changes the guest's path, so the timeline says so.
    fn decline_ring(&mut self, ctx: &mut StopCtx<'_, '_>) -> Result<i64, KernelError> {
        if self.declined_ring {
            return Ok(-errno::ENOSYS);
        }
        self.declined_ring = true;
        ctx.emit(EventKind::Diagnostic {
            code: "BOHR-2004".to_owned(),
            message: "io_uring is declined; the guest runs its fallback path, which is not the \
                      path it runs where a ring is available"
                .to_owned(),
        })?;
        Ok(-errno::ENOSYS)
    }

    /// What to do about a syscall this model has a row for and answers "not implemented".
    ///
    /// Some calls answer `ENOSYS` on purpose so the caller falls back. A survey counts them with
    /// their kind; a run answers silently.
    fn unimplemented(&mut self, ctx: &mut StopCtx<'_, '_>, nr: u32) -> Result<i64, KernelError> {
        if let Some(seen) = self.surveying.as_mut() {
            let count = seen.entry(nr).or_default();
            *count += 1;
            if *count == 1 {
                ctx.emit(EventKind::Diagnostic {
                    code: "BOHR-2001".to_owned(),
                    message: format!("syscall {nr} is answered ENOSYS by this model"),
                })?;
            }
        }
        Ok(-errno::ENOSYS)
    }

    /// Copies out what the guest wrote and reports how much of it was accepted.
    ///
    /// The guest is told every byte was accepted, whatever the sink did.
    fn write_out(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        addr: GuestAddr,
        len: usize,
    ) -> Result<(i64, Option<[u8; 8]>), KernelError> {
        let len = len.min(MAX_TRANSFER);
        self.transfer.clear();
        self.transfer.resize(len, 0);
        ctx.read_mem(addr, &mut self.transfer)?;
        if let Some(sink) = self.output.as_mut() {
            let _ = sink.write_all(&self.transfer);
            let _ = sink.flush();
        }
        // Inline, to avoid copying the transfer buffer.
        if let Some(kept) = self.stdout.as_mut() {
            let room = crate::MAX_PROJECTION_BYTES.saturating_sub(kept.len());
            kept.extend_from_slice(&self.transfer[..room.min(self.transfer.len())]);
        }
        if let Some(tails) = self.tails.as_mut() {
            tails.wrote(0, &self.transfer);
        }
        Ok((len as i64, Some(short_digest(&self.transfer))))
    }

    /// The segments a vector names, as `(base, length)`, read out of the guest in one crossing.
    ///
    /// At most `MAX_IOVECS`, and none past the end of the address space.
    fn segments(
        ctx: &mut StopCtx<'_, '_>,
        iov: GuestAddr,
        count: u64,
    ) -> Result<Vec<(u64, u64)>, KernelError> {
        let room = (u128::from(u64::MAX) - u128::from(iov.0) + 1) / IOVEC_BYTES as u128;
        let count = u128::from(count.min(MAX_IOVECS)).min(room) as usize;
        let mut array = vec![0u8; count * IOVEC_BYTES];
        ctx.read_mem(iov, &mut array)?;
        let (entries, _) = array.as_chunks::<IOVEC_BYTES>();
        Ok(entries
            .iter()
            .map(|entry| {
                let (base, len) = entry.split_at(8);
                let word = |half: &[u8]| u64::from_le_bytes(half.try_into().unwrap_or_default());
                (word(base), word(len))
            })
            .collect())
    }

    /// How many bytes a vector of segments asks for, without copying any of them.
    pub(crate) fn iov_bytes(
        ctx: &mut StopCtx<'_, '_>,
        iov: GuestAddr,
        count: u64,
    ) -> Result<u32, KernelError> {
        let total = Self::segments(ctx, iov, count)?
            .iter()
            .fold(0u64, |total, (_, len)| total.saturating_add(*len));
        Ok(total.min(MAX_TRANSFER as u64) as u32)
    }

    /// Joins the segments a gathering write names into one buffer, up to the transfer bound; the
    /// caller gets a short write for the rest.
    ///
    /// Two crossings whatever the count: the vector, then every segment's bytes. An empty segment
    /// is passed over, as Linux passes it, and the bytes after it are written.
    pub(crate) fn gather(
        ctx: &mut StopCtx<'_, '_>,
        iov: GuestAddr,
        count: u64,
    ) -> Result<Vec<u8>, KernelError> {
        let mut pieces = Vec::new();
        let mut total = 0usize;
        for (base, len) in Self::segments(ctx, iov, count)? {
            let take = usize::try_from(len).unwrap_or(usize::MAX).min(MAX_TRANSFER - total);
            if take > 0 {
                pieces.push((GuestAddr(base), take));
                total += take;
            }
            if total == MAX_TRANSFER {
                break;
            }
        }
        let mut joined = vec![0u8; total];
        ctx.read_pieces(&pieces, &mut joined)?;
        Ok(joined)
    }

    /// Writes bytes already gathered, at a named offset or wherever the descriptor stands.
    ///
    /// A named offset leaves the descriptor's position unchanged.
    fn write_bytes(&mut self, fd: i64, bytes: &[u8], at: Option<u64>) -> (i64, Option<[u8; 8]>) {
        // A standard descriptor counts as standard only until a file is duplicated onto it.
        if self.file_behind(fd).is_none() && (fd == 1 || fd == 2) {
            if let Some(sink) = self.output.as_mut() {
                let _ = sink.write_all(bytes);
                let _ = sink.flush();
            }
            // The same buffers `write_out` fills.
            self.keep_written_out(bytes);
            if let Some(tails) = self.tails.as_mut() {
                tails.wrote(0, bytes);
            }
            return (bytes.len() as i64, Some(short_digest(bytes)));
        }
        let Some(file) = self.open.get(&fd).copied() else {
            return (-errno::EBADF, None);
        };
        let Some(inode) = file.inode else {
            return (bytes.len() as i64, Some(short_digest(bytes)));
        };
        let start = self.write_offset(fd, at) as usize;
        if grown_end(start, bytes.len()).is_none() {
            return (-errno::EFBIG, None);
        }
        self.content.entry(inode).or_default().write_at(start, bytes);
        // The position follows the bytes written, from wherever they landed.
        if at.is_none()
            && let Some(open) = self.open.get_mut(&fd)
        {
            open.position = (start + bytes.len()) as u64;
        }
        (bytes.len() as i64, Some(short_digest(bytes)))
    }

    /// Opens a path, creating it where the flags ask for that.
    fn open_at(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        dirfd: i64,
        addr: GuestAddr,
        flags: u64,
    ) -> Result<i64, KernelError> {
        let path = match read_c_string(ctx, addr)? {
            PathArg::Named(name) => match self.under(dirfd, &name) {
                Ok(path) => self.resolve(&path),
                Err(errno) => return Ok(errno),
            },
            other => return Ok(other.errno()),
        };

        // Generated `/proc` files are marked `synthetic`, kept off every disk image.
        if !self.names.contains_key(&path)
            && let Some(content) = generated(&path)
        {
            self.make_parents(&path);
            let inode = self.next_inode;
            self.next_inode += 1;
            self.names.insert(path.clone(), inode);
            self.content.insert(inode, FileBytes::from_vec(content));
            self.synthetic.insert(inode);
        }

        let inode = if RANDOM_PATHS.contains(&path.as_str()) {
            if flags & O_DIRECTORY != 0 {
                return Ok(-errno::ENOTDIR);
            }
            None
        } else {
            let inode = match self.names.get(&path) {
                Some(inode) => {
                    let inode = *inode;
                    // `O_EXCL` on an existing name is `EEXIST`, as lock files rely on.
                    if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
                        return Ok(-errno::EEXIST);
                    }
                    // A directory opens read-only.
                    if self.directories.contains(&inode) && flags & O_WRITABLE != 0 {
                        return Ok(-errno::EISDIR);
                    }
                    if flags & O_DIRECTORY != 0 && !self.directories.contains(&inode) {
                        return Ok(-errno::ENOTDIR);
                    }
                    inode
                }
                None => {
                    if flags & O_CREAT == 0 || flags & O_DIRECTORY != 0 {
                        return Ok(-errno::ENOENT);
                    }
                    // Creating a file needs its parent directory.
                    if !self.parent_exists(&path) {
                        return Ok(-errno::ENOENT);
                    }
                    let inode = self.next_inode;
                    self.next_inode += 1;
                    self.names.insert(path.clone(), inode);
                    self.content.insert(inode, FileBytes::default());
                    inode
                }
            };
            if flags & O_TRUNC != 0 && !self.directories.contains(&inode) {
                self.content.insert(inode, FileBytes::default());
            }
            Some(inode)
        };

        let fd = self.attach(OpenFile {
            inode,
            position: 0,
            durable: flags & O_WRITE_THROUGH != 0,
            // `O_APPEND` is kept on the descriptor.
            appending: flags & O_APPEND != 0,
            status: flags & status::OPENED | status::LARGEFILE,
        });
        if flags & O_CLOEXEC != 0 {
            self.cloexec.insert(fd);
        }
        if inode.is_some_and(|inode| self.directories.contains(&inode)) {
            self.note_directory(fd, &path);
        }
        Ok(fd)
    }

    /// Answers `getdents64`: what a directory holds, in as much of it as the buffer takes.
    ///
    /// The descriptor's position is the next entry's index. Entries are sorted, so listings
    /// repeat.
    fn list_directory(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        fd: i64,
        addr: GuestAddr,
        capacity: usize,
    ) -> Result<i64, KernelError> {
        let Some(file) = self.open.get(&fd).copied() else {
            return Ok(-errno::EBADF);
        };
        let Some(inode) = file.inode.filter(|inode| self.directories.contains(inode)) else {
            return Ok(-errno::ENOTDIR);
        };
        let Some(path) = self.name_of_inode(inode) else {
            return Ok(-errno::ENOENT);
        };
        let entries = self.entries_of(&path, inode);

        let mut out = Vec::new();
        let mut index = file.position as usize;
        while let Some((name, of, kind)) = entries.get(index) {
            // The name and the byte that ends it, rounded up to where the next one may start.
            let length = (DIRENT_HEADER_BYTES + name.len() + 1).next_multiple_of(8);
            if out.len() + length > capacity {
                // A buffer too small for one entry is `EINVAL`.
                if out.is_empty() {
                    return Ok(-errno::EINVAL);
                }
                break;
            }
            let start = out.len();
            out.resize(start + length, 0);
            put(&mut out, start, &of.to_le_bytes());
            put(&mut out, start + 8, &((index as u64 + 1).to_le_bytes()));
            put(&mut out, start + 16, &(length as u16).to_le_bytes());
            out[start + 18] = *kind;
            put(&mut out, start + 19, name.as_bytes());
            index += 1;
        }

        if let Some(open) = self.open.get_mut(&fd) {
            open.position = index as u64;
        }
        if !out.is_empty() {
            ctx.write_mem(addr, &out)?;
        }
        Ok(out.len() as i64)
    }

    /// What a directory holds, as `(name, inode, kind)`, itself and its parent first.
    fn entries_of(&self, path: &str, inode: u64) -> Vec<(String, u64, u8)> {
        let parent =
            parent_of(path).and_then(|parent| self.names.get(parent)).copied().unwrap_or(inode);
        let mut entries = vec![
            (".".to_owned(), inode, dirent::DIRECTORY),
            ("..".to_owned(), parent, dirent::DIRECTORY),
        ];
        entries.extend(self.names.iter().filter_map(|(name, held)| {
            if parent_of(name) != Some(path) {
                return None;
            }
            let last = name.rsplit('/').next()?;
            let kind =
                if self.directories.contains(held) { dirent::DIRECTORY } else { dirent::REGULAR };
            Some((last.to_owned(), *held, kind))
        }));
        entries
    }

    /// The name a number is known by, for the one caller that has the number and needs the name.
    fn name_of_inode(&self, inode: u64) -> Option<String> {
        self.names.iter().find(|(_, held)| **held == inode).map(|(name, _)| name.clone())
    }

    /// Makes a directory, answering what `mkdir` returns.
    fn make_directory(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        dirfd: i64,
        addr: GuestAddr,
    ) -> Result<i64, KernelError> {
        let path = match read_c_string(ctx, addr)? {
            PathArg::Named(name) => match self.under(dirfd, &name) {
                Ok(path) => path,
                Err(errno) => return Ok(errno),
            },
            other => return Ok(other.errno()),
        };
        let path = path.strip_suffix('/').map_or(path.clone(), str::to_owned);
        if self.names.contains_key(&path) {
            return Ok(-errno::EEXIST);
        }
        if !self.parent_exists(&path) {
            return Ok(-errno::ENOENT);
        }
        let inode = self.next_inode;
        self.next_inode += 1;
        self.names.insert(path, inode);
        self.directories.insert(inode);
        Ok(0)
    }

    /// Whether the directory a name would sit in exists.
    ///
    /// The parent is the text before the last separator.
    fn parent_exists(&self, path: &str) -> bool {
        let Some(parent) = parent_of(path) else {
            return true;
        };
        self.names.get(parent).is_some_and(|inode| self.directories.contains(inode))
    }

    /// Replaces a mapping of a file with one of nothing, and remembers what it was for.
    ///
    /// The host supplies the memory; the world writes the contents when the call returns.
    ///
    /// The mapping is made writable so the supervisor can fill it. A guest writing to a read-only
    /// mapping is therefore not faulted, which this model misses.
    fn map_file(&mut self, ctx: &mut StopCtx<'_, '_>, args: [u64; 6]) -> Option<Resume> {
        const PROT_WRITE: u64 = 0x2;
        const PROT_READ_WRITE: u64 = 0x1 | 0x2;
        const MAP_SHARED: u64 = 0x1;
        const MAP_PRIVATE: u64 = 0x2;
        const MAP_ANONYMOUS: u64 = 0x20;

        const PROT_EXEC: u64 = 0x4;

        // A ring region, named by its offset.
        if let Some(held) = self.rings.get(&(args[4] as i64)).copied() {
            let bytes = held.region_bytes(args[5])?;
            self.mapping.insert(
                ctx.task(),
                PendingMap {
                    len: bytes.max(args[1]),
                    inode: 0,
                    offset: args[5],
                    writable: true,
                    fd: args[4] as i64,
                    ring: Some(args[5]),
                },
            );
            return Some(Resume::Rewrite {
                nr: sys::MMAP,
                args: [
                    args[0],
                    args[1],
                    PROT_READ_WRITE,
                    (args[3] & !MAP_SHARED) | MAP_PRIVATE | MAP_ANONYMOUS,
                    u64::MAX,
                    0,
                ],
            });
        }

        let inode = self.open.get(&(args[4] as i64))?.inode?;
        let writable = args[2] & PROT_WRITE != 0 && args[3] & MAP_SHARED != 0;
        // Execute permission is kept, for `dlopen`; write is still forced.
        let keep_exec = args[2] & PROT_EXEC;
        self.mapping.insert(
            ctx.task(),
            PendingMap {
                len: args[1],
                inode,
                offset: args[5],
                writable,
                fd: args[4] as i64,
                ring: None,
            },
        );
        Some(Resume::Rewrite {
            nr: sys::MMAP,
            args: [
                args[0],
                args[1],
                PROT_READ_WRITE | keep_exec,
                (args[3] & !MAP_SHARED) | MAP_PRIVATE | MAP_ANONYMOUS,
                u64::MAX,
                0,
            ],
        })
    }

    /// Takes the address a replaced mapping landed at and fills it with the file it stands for.
    fn mapping_placed(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        ret: i64,
    ) -> Result<Effects, KernelError> {
        let Some(pending) = self.mapping.remove(&ctx.task()) else {
            return Ok(Effects::go(Resume::Continue, Tick::ZERO));
        };
        // A refused mapping is forgotten.
        if ret < 0 {
            return Ok(Effects::go(Resume::Continue, Tick::ZERO));
        }
        // A ring region gets its control block written in.
        if let Some(region) = pending.ring {
            let at = ret as u64;
            if let Some(held) = self.rings.get_mut(&pending.fd) {
                held.placed(region, at);
                let held = *held;
                match region {
                    ring::offset::SQ_RING => ctx.write_mem(GuestAddr(at), &held.sq_control())?,
                    ring::offset::CQ_RING => ctx.write_mem(GuestAddr(at), &held.cq_control())?,
                    _ => {}
                }
            }
            return Ok(Effects::go(Resume::Continue, SYSCALL_COST));
        }
        let placed = Mapping {
            at: ret as u64,
            len: pending.len,
            inode: pending.inode,
            offset: pending.offset,
            writable: pending.writable,
            fd: pending.fd,
        };
        self.mappings.insert(placed.at, placed);
        self.show_range(ctx, placed, 0..u64::MAX)?;
        ctx.emit(EventKind::Diagnostic {
            code: "BOHR-2005".to_owned(),
            message: format!(
                "mapped {} bytes of inode {} at {:#x}{}",
                placed.len,
                placed.inode,
                placed.at,
                if placed.writable { ", written through" } else { "" }
            ),
        })?;
        Ok(Effects::go(Resume::Continue, Tick::ZERO))
    }

    /// Writes what the world holds of a file into one mapping of it.
    ///
    /// Past the file's end the mapping stays zero.
    fn show_range(
        &self,
        ctx: &mut StopCtx<'_, '_>,
        mapping: Mapping,
        want: std::ops::Range<u64>,
    ) -> Result<(), KernelError> {
        let Some(content) = self.content.get(&mapping.inode) else {
            return Ok(());
        };
        let held = content.len() as u64;
        let from = want.start.max(mapping.offset);
        let to = want.end.min(mapping.offset.saturating_add(mapping.len)).min(held);
        if from >= to {
            return Ok(());
        }
        for (offset, part) in content.segments(from as usize..to as usize) {
            let at = mapping.at.saturating_add(offset as u64 - mapping.offset);
            ctx.write_mem(GuestAddr(at), part)?;
        }
        Ok(())
    }

    /// Shows every mapping of a file what changed in it, in the file's own terms.
    ///
    /// Only the changed range, since stores map windows far larger than the file.
    fn refresh_mappings(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        inode: u64,
        want: std::ops::Range<u64>,
    ) -> Result<(), KernelError> {
        let showing: Vec<Mapping> =
            self.mappings.values().filter(|m| m.inode == inode).copied().collect();
        for mapping in showing {
            self.show_range(ctx, mapping, want.clone())?;
        }
        Ok(())
    }

    /// Takes back what the guest wrote through its mappings of a file.
    ///
    /// A store to mapped memory makes no syscall, so this is how the world learns of it.
    fn collect_mappings(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        inode: u64,
    ) -> Result<(), KernelError> {
        let written: Vec<Mapping> =
            self.mappings.values().filter(|m| m.writable && m.inode == inode).copied().collect();
        for mapping in written {
            // Only up to the file's end; beyond it is `SIGBUS` territory.
            let held = self.content.get(&mapping.inode).map_or(0, |c| c.len()) as u64;
            let take = mapping.len.min(held.saturating_sub(mapping.offset));
            if take == 0 {
                continue;
            }
            let mut bytes = vec![0u8; take as usize];
            ctx.read_mem(GuestAddr(mapping.at), &mut bytes)?;
            self.content
                .entry(mapping.inode)
                .or_default()
                .write_changed(mapping.offset as usize, &bytes);
        }
        Ok(())
    }

    /// Whether a file is written through a mapping, and so has to be collected before it is read.
    fn is_mapped_writable(&self, inode: u64) -> bool {
        self.mappings.values().any(|m| m.writable && m.inode == inode)
    }

    /// The file an `msync` is about, and the descriptor to narrate it against.
    ///
    /// `Err` when the range covers written mappings of several files, since one sync has one fate
    /// (`docs/API.md` §2.4).
    pub(crate) fn mapped_sync(&self, at: u64, len: u64) -> Result<Option<(i64, u64)>, ()> {
        let end = at.saturating_add(len);
        let mut found: Option<(i64, u64)> = None;
        for mapping in self.mappings.values() {
            if !mapping.writable
                || mapping.at >= end
                || mapping.at.saturating_add(mapping.len) <= at
            {
                continue;
            }
            match found {
                Some((_, inode)) if inode != mapping.inode => return Err(()),
                Some(_) => {}
                None => found = Some((mapping.fd, mapping.inode)),
            }
        }
        Ok(found)
    }

    /// Carries back what the guest wrote through its mappings of one file.
    pub(crate) fn collect_mapped(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        inode: u64,
    ) -> Result<Vec<(u64, u64)>, KernelError> {
        let ranges: Vec<(u64, u64)> = self
            .mappings
            .values()
            .filter(|m| m.writable && m.inode == inode)
            .map(|m| (m.offset, m.len))
            .collect();
        self.collect_mappings(ctx, inode)?;
        Ok(ranges)
    }

    /// Forgets the mappings an address range covered, taking back what they held first.
    fn unmap(&mut self, ctx: &mut StopCtx<'_, '_>, at: u64, len: u64) -> Result<(), KernelError> {
        let going: Vec<Mapping> = self
            .mappings
            .values()
            .filter(|m| m.at >= at && m.at < at.saturating_add(len))
            .copied()
            .collect();
        for mapping in going {
            if mapping.writable {
                self.collect_mappings(ctx, mapping.inode)?;
            }
            self.mappings.remove(&mapping.at);
        }
        Ok(())
    }

    /// What a mapping was asked for, in the terms that decide whether it can be modelled.
    ///
    /// Written into the refusal.
    fn describe_mapping(&self, args: [u64; 6]) -> String {
        const PROT_WRITE: u64 = 0x2;
        const MAP_SHARED: u64 = 0x1;

        let fd = args[4] as i64;
        let name = self
            .open
            .get(&fd)
            .and_then(|f| f.inode)
            .and_then(|inode| self.names.iter().find(|(_, held)| **held == inode))
            .map_or_else(|| format!("descriptor {fd}"), |(name, _)| name.clone());
        let sharing = if args[3] & MAP_SHARED == 0 { "private" } else { "shared" };
        let access = if args[3] & PROT_WRITE == 0 && args[2] & PROT_WRITE == 0 {
            "read-only"
        } else {
            "writable"
        };
        format!("{name}, {} bytes at offset {}, {sharing} and {access}", args[1], args[5])
    }

    /// The same world, with somewhere to put a file it is going to be asked for.
    ///
    /// For a channel the world offers, whose directory the guest did not choose.
    #[must_use]
    pub fn providing(mut self, path: &str) -> Self {
        self.make_parents(path);
        self
    }

    /// Makes every directory a path sits under, from the root down.
    fn make_parents(&mut self, path: &str) {
        let mut cut = 0;
        while let Some(next) = path[cut + 1..].find('/') {
            cut += next + 1;
            self.add_directory(&path[..cut]);
        }
    }

    /// Records a directory in a world being built from a prepared image.
    pub(crate) fn add_directory(&mut self, path: &str) {
        if self.names.contains_key(path) {
            return;
        }
        let inode = self.next_inode;
        self.next_inode += 1;
        self.names.insert(path.to_owned(), inode);
        self.directories.insert(inode);
    }

    /// Gives a descriptor the lowest free number, as the kernel does.
    ///
    /// Programs redirect by number and rely on this.
    fn attach(&mut self, file: OpenFile) -> i64 {
        self.attach_from(file, FIRST_FREE_FD)
    }

    /// Gives a descriptor the lowest free number at or above `at_least`, as `F_DUPFD` asks.
    fn attach_from(&mut self, file: OpenFile, at_least: i64) -> i64 {
        let fd = self.lowest_free(at_least);
        self.open.insert(fd, file);
        fd
    }

    /// The lowest number at or above `at_least` that no descriptor holds, of any kind.
    fn lowest_free(&self, at_least: i64) -> i64 {
        let mut fd = at_least.max(0);
        while self.open.contains_key(&fd) || self.foreign.contains(&fd) || fd == RESERVED_FD {
            fd += 1;
        }
        fd
    }

    /// Takes the lowest free number at or above `at_least` for a descriptor kept elsewhere.
    pub(crate) fn claim_number(&mut self, at_least: i64) -> i64 {
        let fd = self.lowest_free(at_least);
        self.foreign.insert(fd);
        fd
    }

    /// Takes `fd` for a descriptor kept elsewhere; the caller has closed whatever held it.
    pub(crate) fn claim_exact(&mut self, fd: i64) {
        self.foreign.insert(fd);
    }

    /// Gives back a number a descriptor kept elsewhere held.
    pub(crate) fn release_number(&mut self, fd: i64) {
        self.foreign.remove(&fd);
    }

    /// Closes a file descriptor, answering 0, or `EBADF` where it names no file.
    ///
    /// The content goes once no name and no descriptor holds it.
    pub(crate) fn close_number(&mut self, fd: i64) -> i64 {
        self.dir_of.remove(&fd);
        self.cloexec.remove(&fd);
        self.rings.remove(&fd);
        match self.open.remove(&fd) {
            Some(file) => {
                if let Some(inode) = file.inode {
                    self.collect(inode);
                }
                0
            }
            None => -errno::EBADF,
        }
    }

    /// Remembers where a descriptor opened on a directory sits.
    ///
    /// Only directories, since only they are named by later calls.
    fn note_directory(&mut self, fd: i64, path: &str) {
        self.dir_of.insert(fd, path.to_owned());
    }

    /// Reads into the guest, either from a stored file or from the endless random source.
    fn read_in(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        fd: i64,
        addr: GuestAddr,
        len: usize,
        at: Option<u64>,
    ) -> Result<(i64, Option<[u8; 8]>), KernelError> {
        // Standard streams read as end of file.
        if (0..=2).contains(&fd) {
            return Ok((0, None));
        }
        let Some(file) = self.open.get(&fd).copied() else {
            return Ok((-errno::EBADF, None));
        };
        let len = len.min(MAX_TRANSFER);

        let Some(inode) = file.inode else {
            // Every read of the random source is fresh tape.
            self.transfer.clear();
            self.transfer.resize(len, 0);
            ctx.draw_into(&mut self.transfer, DrawSite::GuestRandom);
            ctx.write_mem(addr, &self.transfer)?;
            return Ok((len as i64, Some(short_digest(&self.transfer))));
        };

        let from = usize::try_from(at.unwrap_or(file.position)).unwrap_or(usize::MAX);
        self.transfer.clear();
        if let Some(content) = self.content.get(&inode) {
            let take = content.len().saturating_sub(from).min(len);
            for (_, part) in content.segments(from..from.saturating_add(take)) {
                self.transfer.extend_from_slice(part);
            }
        }
        let take = self.transfer.len();
        ctx.write_mem(addr, &self.transfer)?;
        if at.is_none()
            && let Some(open) = self.open.get_mut(&fd)
        {
            open.position += take as u64;
        }
        Ok((take as i64, Some(short_digest(&self.transfer))))
    }

    /// Writes into a stored file, extending it where the write lands past its end.
    fn write_file(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        fd: i64,
        addr: GuestAddr,
        len: usize,
        at: Option<u64>,
    ) -> Result<(i64, Option<[u8; 8]>), KernelError> {
        // An unheld descriptor is refused before reading guest memory.
        if !self.open.contains_key(&fd) {
            return Ok((-errno::EBADF, None));
        }

        let len = len.min(MAX_TRANSFER);
        self.transfer.clear();
        self.transfer.resize(len, 0);
        ctx.read_mem(addr, &mut self.transfer)?;

        // `write_bytes` does growth, position and append in one place (rule 7).
        let bytes = std::mem::take(&mut self.transfer);
        let answer = self.write_bytes(fd, &bytes, at);
        self.transfer = bytes;
        Ok(answer)
    }

    /// Sets the length of a stored file, filling with zeroes where it grows.
    fn truncate(&mut self, fd: i64, length: u64) -> i64 {
        let Some(Some(inode)) = self.open.get(&fd).map(|f| f.inode) else {
            return if self.open.contains_key(&fd) { 0 } else { -errno::EBADF };
        };
        let Ok(length) = usize::try_from(length) else {
            return -errno::EINVAL;
        };
        if length > MAX_FILE_BYTES {
            return -errno::EFBIG;
        }
        self.content.entry(inode).or_default().resize(length);
        // A truncate frees every block past the new end, reserved ones too.
        if self.reserved.get(&inode).is_some_and(|end| *end > length as u64) {
            self.reserved.remove(&inode);
        }
        0
    }

    /// Whether `len` bytes at `offset` through `fd` need space the file does not hold yet: past
    /// its end and past what `fallocate` reserved.
    ///
    /// Overwriting what a file holds never needs space, as on ext4. A hole inside a sparse file
    /// counts as held, where ext4 would allocate it on write.
    pub(crate) fn needs_space(&self, fd: i64, offset: u64, len: u64) -> bool {
        let Some(Some(inode)) = self.open.get(&fd).map(|f| f.inode) else {
            return false;
        };
        let length = self.content.get(&inode).map_or(0, |held| held.len() as u64);
        let held = length.max(self.reserved.get(&inode).copied().unwrap_or(0));
        len > 0 && offset.saturating_add(len) > held
    }

    /// Whether any name or link sits under `path`.
    fn has_children(&self, path: &str) -> bool {
        let prefix = format!("{path}/");
        self.names.keys().chain(self.links.keys()).any(|name| name.starts_with(&prefix))
    }

    /// Moves `from` and everything under it to the same places under `to`.
    ///
    /// Names are full paths, so a directory's move is its subtree's: the names, the links among
    /// them, the directories descriptors stand in, and the working directory all follow.
    fn move_subtree(&mut self, from: &str, to: &str) {
        let moved = |path: &str| -> Option<String> {
            if path == from {
                return Some(to.to_owned());
            }
            let rest = path.strip_prefix(from)?;
            rest.starts_with('/').then(|| format!("{to}{rest}"))
        };
        let names: Vec<(String, String, u64)> = self
            .names
            .iter()
            .filter_map(|(path, inode)| moved(path).map(|new| (path.clone(), new, *inode)))
            .collect();
        for (old, ..) in &names {
            self.names.remove(old);
        }
        for (_, new, inode) in names {
            self.names.insert(new, inode);
        }
        let links: Vec<(String, String, String)> = self
            .links
            .iter()
            .filter_map(|(path, text)| moved(path).map(|new| (path.clone(), new, text.clone())))
            .collect();
        for (old, ..) in &links {
            self.links.remove(old);
        }
        for (_, new, text) in links {
            self.links.insert(new, text);
        }
        for path in self.dir_of.values_mut() {
            if let Some(new) = moved(path) {
                *path = new;
            }
        }
        if let Some(new) = moved(&self.cwd) {
            self.cwd = new;
        }
    }

    /// Moves a symbolic link to another name, as `rename` moves any name.
    ///
    /// It replaces a file or a link there, never a directory.
    fn rename_link(&mut self, from: &str, to: &str, no_replace: bool, exchange: bool) -> i64 {
        let held = self.names.get(to).copied();
        let taken = held.is_some() || self.links.contains_key(to);
        if exchange {
            if !taken {
                return -errno::ENOENT;
            }
            const PASSING: &str = "\0exchange";
            self.move_subtree(from, PASSING);
            self.move_subtree(to, from);
            self.move_subtree(PASSING, to);
            return 0;
        }
        if taken && no_replace {
            return -errno::EEXIST;
        }
        if let Some(replaced) = held {
            if self.directories.contains(&replaced) {
                return -errno::EISDIR;
            }
            self.names.remove(to);
            self.collect(replaced);
        }
        if let Some(text) = self.links.remove(from) {
            self.links.insert(to.to_owned(), text);
        }
        0
    }

    /// Moves a stored file or directory to another name, replacing whatever was there, as Linux
    /// `rename` does.
    ///
    /// A directory moves with everything under it, onto nothing or onto an empty directory, and
    /// never into itself. A file never replaces a directory, nor a directory a file. Two names of
    /// one file leave both in place.
    fn rename(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        dirs: (i64, i64),
        from: GuestAddr,
        to: GuestAddr,
        flags: u64,
    ) -> Result<i64, KernelError> {
        /// `RENAME_NOREPLACE`: fail if the name is taken.
        const NOREPLACE: u64 = 1 << 0;
        /// `RENAME_EXCHANGE`: the two names swap what they point at.
        const EXCHANGE: u64 = 1 << 1;

        let from = match read_c_string(ctx, from)? {
            PathArg::Named(name) => match self.under(dirs.0, &name) {
                Ok(path) => path,
                Err(errno) => return Ok(errno),
            },
            other => return Ok(other.errno()),
        };
        let to = match read_c_string(ctx, to)? {
            PathArg::Named(name) => match self.under(dirs.1, &name) {
                Ok(path) => path,
                Err(errno) => return Ok(errno),
            },
            other => return Ok(other.errno()),
        };
        let (from, to) = (self.absolute(&from), self.absolute(&to));
        // Unknown flags are refused.
        if flags & !(NOREPLACE | EXCHANGE) != 0 {
            return Ok(-errno::EINVAL);
        }
        if self.links.contains_key(&from) {
            return Ok(self.rename_link(&from, &to, flags & NOREPLACE != 0, flags & EXCHANGE != 0));
        }
        let Some(inode) = self.names.get(&from).copied() else {
            return Ok(-errno::ENOENT);
        };
        // The destination's parent must exist, as for `openat`.
        if !self.parent_exists(&to) {
            return Ok(-errno::ENOENT);
        }
        // The root is not moved, nor replaced.
        if from == ROOT || to == ROOT {
            return Ok(-errno::EBUSY);
        }
        // Neither name may sit under the other: a directory never moves into itself.
        let under = |inner: &str, outer: &str| inner.starts_with(&format!("{outer}/"));
        if under(&to, &from) || (flags & EXCHANGE != 0 && under(&from, &to)) {
            return Ok(-errno::EINVAL);
        }
        let held = self.names.get(&to).copied();
        let held_link = self.links.contains_key(&to);
        if flags & EXCHANGE != 0 {
            if held.is_none() && !held_link {
                return Ok(-errno::ENOENT);
            }
            // Through a name no path can take, so the two subtrees never meet.
            const PASSING: &str = "\0exchange";
            self.move_subtree(&from, PASSING);
            self.move_subtree(&to, &from);
            self.move_subtree(PASSING, &to);
            return Ok(0);
        }
        // The caller said the name must be free, and it is not.
        if (held.is_some() || held_link) && flags & NOREPLACE != 0 {
            return Ok(-errno::EEXIST);
        }
        // A link in the way is replaced by a file, never by a directory.
        if held_link {
            if self.directories.contains(&inode) {
                return Ok(-errno::ENOTDIR);
            }
            self.links.remove(&to);
        }
        // Two names of one file: nothing moves.
        if held == Some(inode) {
            return Ok(0);
        }
        let moving_directory = self.directories.contains(&inode);
        if let Some(replaced) = held {
            match (moving_directory, self.directories.contains(&replaced)) {
                (true, false) => return Ok(-errno::ENOTDIR),
                (false, true) => return Ok(-errno::EISDIR),
                (true, true) if self.has_children(&to) => return Ok(-errno::ENOTEMPTY),
                _ => {}
            }
            self.names.remove(&to);
        }
        self.move_subtree(&from, &to);
        // The replaced content is freed if nothing else names it.
        if let Some(replaced) = held {
            self.directories.remove(&replaced);
            self.collect(replaced);
        }
        Ok(0)
    }

    /// Writes each entry's `revents` and says how many are ready, or the error the array earned.
    fn poll_ready(
        &self,
        ctx: &mut StopCtx<'_, '_>,
        addr: GuestAddr,
        count: u64,
    ) -> Result<i64, KernelError> {
        /// What a file is always ready for: `POLLIN`, `POLLOUT`, `POLLRDNORM`, `POLLWRNORM`.
        const ALWAYS: u16 = 0x1 | 0x4 | 0x40 | 0x100;
        /// `POLLNVAL`.
        const INVALID: u16 = 0x20;
        let Ok(count) = usize::try_from(count) else { return Ok(-errno::EINVAL) };
        if count > MAX_POLL_ENTRIES {
            return Ok(-errno::EINVAL);
        }
        let mut ready = 0;
        for i in 0..count {
            // The whole entry must fit in the address space.
            let (Some(at), Some(_)) = (
                offset(addr, (i * POLLFD_BYTES) as u64),
                offset(addr, ((i + 1) * POLLFD_BYTES) as u64),
            ) else {
                return Ok(-errno::EFAULT);
            };
            let mut entry = [0u8; POLLFD_BYTES];
            ctx.read_mem(at, &mut entry)?;
            let fd = i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
            let events = u16::from_le_bytes([entry[4], entry[5]]);
            // A negative descriptor is an entry the caller switched off, and reports nothing.
            let revents = if fd < 0 {
                0
            } else if self.open.contains_key(&i64::from(fd)) {
                events & ALWAYS
            } else {
                INVALID
            };
            if revents != 0 {
                ready += 1;
            }
            let Some(out) = offset(at, 6) else { return Ok(-errno::EFAULT) };
            ctx.write_mem(out, &revents.to_le_bytes())?;
        }
        Ok(ready)
    }

    /// Copies bytes into a guest buffer, reporting the length or that it did not fit.
    fn write_bytes_out(
        ctx: &mut StopCtx<'_, '_>,
        addr: GuestAddr,
        capacity: u64,
        bytes: &[u8],
    ) -> Result<i64, KernelError> {
        if (capacity as usize) < bytes.len() {
            return Ok(-errno::ERANGE);
        }
        ctx.write_mem(addr, bytes)?;
        Ok(bytes.len() as i64)
    }

    /// Writes the machine description, which is the same on every host.
    fn write_uname(ctx: &mut StopCtx<'_, '_>, addr: GuestAddr) -> Result<i64, KernelError> {
        let mut buf = [0u8; UTSNAME_BYTES];
        for (i, field) in ["Linux", "bohr", PRESENTED_RELEASE, PRESENTED_VERSION, "x86_64", "bohr"]
            .iter()
            .enumerate()
        {
            let at = i * UTSNAME_FIELD;
            buf[at..at + field.len()].copy_from_slice(field.as_bytes());
        }
        ctx.write_mem(addr, &buf)?;
        Ok(0)
    }

    /// Reports the processor set the guest runs on, which is one processor.
    fn write_affinity(ctx: &mut StopCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
        let size = usize::try_from(args[1]).unwrap_or(0).min(128);
        if size < 8 {
            return Ok(-errno::EINVAL);
        }
        let mut mask = vec![0u8; size];
        mask[0] = 1;
        ctx.write_mem(GuestAddr(args[2]), &mask)?;
        Ok(size as i64)
    }

    /// Frees content that has neither a name nor an open descriptor left.
    ///
    /// A file exists while a name or descriptor reaches it.
    fn collect(&mut self, inode: u64) {
        let named = self.names.values().any(|i| *i == inode);
        let held = self.open.values().any(|f| f.inode == Some(inode));
        if !named && !held {
            self.content.remove(&inode);
        }
    }

    /// The length of what a descriptor refers to, or `None` for a descriptor nothing is open on.
    pub(crate) fn length_of(&self, fd: i64) -> Option<u64> {
        let file = self.open.get(&fd)?;
        match file.inode {
            Some(inode) => Some(self.content.get(&inode).map_or(0, |c| c.len() as u64)),
            None => Some(0),
        }
    }

    /// Moves the read and write position of a descriptor.
    fn seek(&mut self, fd: i64, offset: i64, whence: u64) -> i64 {
        let Some(length) = self.length_of(fd) else {
            return -errno::EBADF;
        };
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => self.open.get(&fd).map_or(0, |f| f.position),
            SEEK_END => length,
            _ => return -errno::EINVAL,
        };
        let Some(target) = base.checked_add_signed(offset) else {
            return -errno::EINVAL;
        };
        // A position that does not fit the return value is refused without moving.
        let Ok(answer) = i64::try_from(target) else {
            return -errno::EOVERFLOW;
        };
        if let Some(open) = self.open.get_mut(&fd) {
            open.position = target;
        }
        answer
    }

    /// Writes a `struct stat` for a descriptor, as the guest's C library expects to read it.
    fn write_stat(
        &self,
        ctx: &mut StopCtx<'_, '_>,
        fd: i64,
        addr: GuestAddr,
    ) -> Result<i64, KernelError> {
        let Some(length) = self.length_of(fd) else {
            return Ok(-errno::EBADF);
        };
        let inode = self.open.get(&fd).and_then(|f| f.inode);
        ctx.write_mem(addr, &self.stat_of(inode, length))?;
        Ok(0)
    }

    /// The file a descriptor stands for, where it stands for one.
    ///
    /// The standard descriptors are present but name no file.
    fn file_behind(&self, fd: i64) -> Option<u64> {
        self.open.get(&fd).and_then(|file| file.inode)
    }

    /// Answers `fcntl` on a file, as Linux answers each command.
    ///
    /// A record lock never conflicts: a node runs one process, whose own POSIX locks never stand
    /// in its way, so `F_GETLK` writes `F_UNLCK` and a lock is granted at once. Two open-file
    /// locks on one file, which Linux would let conflict within one process, do not conflict
    /// here. A command Linux does not know is `EINVAL`.
    fn fcntl(&mut self, ctx: &mut StopCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
        let fd = args[0] as i64;
        let Some(file) = self.open.get(&fd).copied() else {
            return Ok(-errno::EBADF);
        };
        let command = args[1] as u32;
        Ok(match command {
            fcntl_cmd::DUPFD | fcntl_cmd::DUPFD_CLOEXEC => {
                let Ok(at_least) = i32::try_from(args[2] as i64) else {
                    return Ok(-errno::EINVAL);
                };
                if at_least < 0 {
                    return Ok(-errno::EINVAL);
                }
                let copy = self.attach_from(file, i64::from(at_least));
                if let Some(dir) = self.dir_of.get(&fd).cloned() {
                    self.dir_of.insert(copy, dir);
                }
                if command == fcntl_cmd::DUPFD_CLOEXEC {
                    self.cloexec.insert(copy);
                }
                copy
            }
            fcntl_cmd::GETFD => i64::from(self.cloexec.contains(&fd)),
            fcntl_cmd::SETFD => {
                if args[2] & fcntl_cmd::FD_CLOEXEC != 0 {
                    self.cloexec.insert(fd);
                } else {
                    self.cloexec.remove(&fd);
                }
                0
            }
            fcntl_cmd::GETFL => file.status as i64,
            fcntl_cmd::SETFL => {
                if let Some(open) = self.open.get_mut(&fd) {
                    open.status = open.status & !status::SETTABLE | args[2] & status::SETTABLE;
                    open.appending = open.status & O_APPEND != 0;
                }
                0
            }
            fcntl_cmd::GETLK | fcntl_cmd::OFD_GETLK => {
                ctx.write_mem(GuestAddr(args[2]), &fcntl_cmd::UNLOCKED.to_le_bytes())?;
                0
            }
            fcntl_cmd::SETLK
            | fcntl_cmd::SETLKW
            | fcntl_cmd::OFD_SETLK
            | fcntl_cmd::OFD_SETLKW
            | fcntl_cmd::SETLEASE
            | fcntl_cmd::NOTIFY => 0,
            fcntl_cmd::GETLEASE => i64::from(fcntl_cmd::UNLOCKED),
            other if fcntl_cmd::OWNERSHIP.contains(&other) => 0,
            other if fcntl_cmd::RW_HINTS.contains(&other) => 0,
            _ => -errno::EINVAL,
        })
    }

    /// Answers `dup`, `dup2` and `dup3`: a second number for a file already open.
    ///
    /// The copies do not share a position, unlike Linux, since descriptors are held by value. This
    /// misses programs that read through one copy and expect the other to move.
    fn duplicate(&mut self, from: i64, onto: Option<i64>, exact: bool, flags: u64) -> i64 {
        let Some(file) = self.open.get(&from).copied() else {
            return -errno::EBADF;
        };
        let Some(onto) = onto else {
            let fd = self.attach(file);
            if let Some(dir) = self.dir_of.get(&from).cloned() {
                self.dir_of.insert(fd, dir);
            }
            return fd;
        };
        if onto < 0 {
            return -errno::EBADF;
        }
        // `dup2` onto itself does nothing; `dup3` calls it an error.
        if from == onto {
            return if exact { -errno::EINVAL } else { onto };
        }
        self.open.insert(onto, file);
        // The copy is marked only where `dup3` asks for it.
        if exact && flags & O_CLOEXEC != 0 {
            self.cloexec.insert(onto);
        } else {
            self.cloexec.remove(&onto);
        }
        // The duplicate names the same directory, replacing what the number named.
        match self.dir_of.get(&from).cloned() {
            Some(dir) => self.dir_of.insert(onto, dir),
            None => self.dir_of.remove(&onto),
        };
        onto
    }

    /// Answers `readlink`: the text a name stands for.
    ///
    /// A link gives its text, a non-link is `EINVAL`, a missing name is `ENOENT`, for both
    /// spellings.
    ///
    /// `/proc/self/exe` answers the configured program. No terminator is written.
    fn read_link(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        dirfd: i64,
        at: GuestAddr,
        buf: GuestAddr,
        len: u64,
    ) -> Result<i64, KernelError> {
        let path = match read_c_string(ctx, at)? {
            PathArg::Named(name) => match self.under(dirfd, &name) {
                Ok(path) => path,
                Err(errno) => return Ok(errno),
            },
            other => return Ok(other.errno()),
        };

        let target = match self.links.get(&path) {
            Some(target) => target.clone(),
            None if path == "/proc/self/exe" || path == format!("/proc/{GUEST_PID}/exe") => {
                // Absolute against the world's working directory; static glibc aborts on a
                // relative answer.
                match self.program.clone() {
                    Some(program) => self.absolute(&program),
                    None => return Ok(-errno::ENOENT),
                }
            }
            None if self.names.contains_key(&path) => return Ok(-errno::EINVAL),
            None => return Ok(-errno::ENOENT),
        };

        // A zero-length buffer is `EINVAL`, as on Linux.
        let room = usize::try_from(len).unwrap_or(usize::MAX);
        if room == 0 {
            return Ok(-errno::EINVAL);
        }
        let written = target.len().min(room);
        ctx.write_mem(buf, &target.as_bytes()[..written])?;
        Ok(written as i64)
    }

    /// Answers `symlink`: a name that stands for a text.
    ///
    /// The text is kept as given, so log rotation works. The target need not exist, as on Linux.
    fn make_link(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        dirfd: i64,
        target: GuestAddr,
        at: GuestAddr,
    ) -> Result<i64, KernelError> {
        let target = match read_c_string(ctx, target)? {
            PathArg::Named(name) => name,
            other => return Ok(other.errno()),
        };
        let path = match read_c_string(ctx, at)? {
            PathArg::Named(name) => match self.under(dirfd, &name) {
                Ok(path) => path,
                Err(errno) => return Ok(errno),
            },
            other => return Ok(other.errno()),
        };
        // An empty target is refused.
        if target.is_empty() {
            return Ok(-errno::ENOENT);
        }
        if self.names.contains_key(&path) || self.links.contains_key(&path) {
            return Ok(-errno::EEXIST);
        }
        self.make_parents(&path);
        self.links.insert(path, target);
        Ok(0)
    }

    /// Answers `newfstatat`: about a name, or about the descriptor itself.
    ///
    /// `stat` has no fallback, so it is always answered.
    fn stat_at(&self, ctx: &mut StopCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
        match self.stat_target(ctx, args[0], args[1], args[3])? {
            Err(errno) => Ok(errno),
            Ok((inode, length)) => {
                ctx.write_mem(GuestAddr(args[2]), &self.stat_of(inode, length))?;
                Ok(0)
            }
        }
    }

    /// Answers `statx`, which a kernel with `io_uring` has: the same facts as `newfstatat`.
    ///
    /// Every basic field is filled and the returned mask says so.
    fn statx_at(&self, ctx: &mut StopCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
        match self.stat_target(ctx, args[0], args[1], args[2])? {
            Err(errno) => Ok(errno),
            Ok((inode, length)) => {
                ctx.write_mem(GuestAddr(args[4]), &self.statx_of(inode, length))?;
                Ok(0)
            }
        }
    }

    /// What a path relative to a descriptor names, as the inode and length a stat reports.
    fn stat_target(
        &self,
        ctx: &mut StopCtx<'_, '_>,
        dirfd: u64,
        name_at: u64,
        flags: u64,
    ) -> Result<Result<(Option<u64>, u64), i64>, KernelError> {
        let fd = dirfd_of(dirfd);
        let name = match read_c_string(ctx, GuestAddr(name_at))? {
            PathArg::Named(name) => name,
            other => return Ok(Err(other.errno())),
        };
        // The empty name is the descriptor itself.
        let path = if name.is_empty() {
            String::new()
        } else {
            match self.under(fd, &name) {
                Ok(path) => self.resolve(&path),
                Err(errno) => return Ok(Err(errno)),
            }
        };
        if path.is_empty() {
            if flags & AT_EMPTY_PATH == 0 {
                return Ok(Err(-errno::ENOENT));
            }
            let Some(length) = self.length_of(fd) else {
                return Ok(Err(-errno::EBADF));
            };
            return Ok(Ok((self.open.get(&fd).and_then(|f| f.inode), length)));
        }
        if RANDOM_PATHS.contains(&path.as_str()) {
            return Ok(Ok((None, 0)));
        }
        let Some(inode) = self.names.get(&path).copied() else {
            return Ok(Err(-errno::ENOENT));
        };
        let length = self.content.get(&inode).map_or(0, |c| c.len()) as u64;
        Ok(Ok((Some(inode), length)))
    }

    /// Answers `statfs` and `fstatfs`: what the filesystem says about itself.
    ///
    /// One device, so the target is checked only for existence. Free space reflects what the world
    /// holds; `ENOSPC` is drawn separately (`docs/API.md` §5.2).
    fn write_statfs(
        &self,
        ctx: &mut StopCtx<'_, '_>,
        named: bool,
        addr: GuestAddr,
    ) -> Result<i64, KernelError> {
        if !named {
            return Ok(-errno::ENOENT);
        }
        let used: u64 = self.content.values().map(|bytes| bytes.len() as u64).sum();
        let free = filesystem::BLOCKS.saturating_sub(used.div_ceil(stat::BLOCK_SIZE));

        // Field offsets are those of `struct statfs` on x86-64.
        let mut buf = [0u8; STATFS_BYTES];
        put(&mut buf, 0, &filesystem::MAGIC.to_le_bytes());
        put(&mut buf, 8, &stat::BLOCK_SIZE.to_le_bytes());
        put(&mut buf, 16, &filesystem::BLOCKS.to_le_bytes());
        put(&mut buf, 24, &free.to_le_bytes());
        put(&mut buf, 32, &free.to_le_bytes());
        put(&mut buf, 40, &filesystem::BLOCKS.to_le_bytes());
        put(
            &mut buf,
            48,
            &filesystem::BLOCKS.saturating_sub(self.names.len() as u64).to_le_bytes(),
        );
        put(&mut buf, 64, &filesystem::NAME_MAX.to_le_bytes());
        put(&mut buf, 72, &stat::BLOCK_SIZE.to_le_bytes());
        ctx.write_mem(addr, &buf)?;
        Ok(0)
    }

    /// The `struct statx` for one object: the same facts as [`Self::stat_of`], in its layout.
    fn statx_of(&self, inode: Option<u64>, length: u64) -> [u8; STATX_BYTES] {
        /// `STATX_BASIC_STATS`: type, mode, links, owner, times, inode, size and blocks.
        const BASIC_STATS: u32 = 0x7ff;
        let mode = self.mode_of(inode);
        let length = if mode == stat::DIRECTORY_MODE { 0 } else { length };

        // Field offsets are those of `struct statx`; the device is split the way `makedev` packs
        // it.
        let mut buf = [0u8; STATX_BYTES];
        put(&mut buf, 0, &BASIC_STATS.to_le_bytes());
        put(&mut buf, 4, &(stat::BLOCK_SIZE as u32).to_le_bytes());
        put(&mut buf, 16, &1u32.to_le_bytes());
        put(&mut buf, 28, &(mode as u16).to_le_bytes());
        put(&mut buf, 32, &inode.unwrap_or(0).to_le_bytes());
        put(&mut buf, 40, &length.to_le_bytes());
        put(&mut buf, 48, &length.div_ceil(512).to_le_bytes());
        let (major, minor) = ((stat::DEVICE >> 8) & 0xfff, stat::DEVICE & 0xff);
        put(&mut buf, 136, &(major as u32).to_le_bytes());
        put(&mut buf, 140, &(minor as u32).to_le_bytes());
        // Access, birth, change and modification, at the epoch, as `stat` says them.
        for at in [64, 80, 96, 112] {
            put(&mut buf, at, &epoch_seconds().to_le_bytes());
        }
        buf
    }

    /// An object's type and permission bits: the bits a guest set, or the ones it was made with.
    fn mode_of(&self, inode: Option<u64>) -> u32 {
        let made = match inode {
            Some(inode) if self.directories.contains(&inode) => stat::DIRECTORY_MODE,
            Some(_) => stat::REGULAR_MODE,
            None => stat::CHAR_MODE,
        };
        match inode.and_then(|i| self.modes.get(&i)) {
            Some(bits) => (made & !0o7777) | bits,
            None => made,
        }
    }

    /// Answers `prctl`, for the options a process sets and reads about itself.
    ///
    /// As for an unprivileged process; options this world does not model are `EINVAL`.
    fn prctl(&mut self, ctx: &mut StopCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
        const PR_SET_PDEATHSIG: u64 = 1;
        const PR_GET_PDEATHSIG: u64 = 2;
        const PR_GET_DUMPABLE: u64 = 3;
        const PR_SET_DUMPABLE: u64 = 4;
        const PR_SET_NAME: u64 = 15;
        const PR_GET_NAME: u64 = 16;
        const PR_GET_TIMERSLACK: u64 = 30;
        const PR_SET_TIMERSLACK: u64 = 29;
        const PR_SET_NO_NEW_PRIVS: u64 = 38;
        const PR_GET_NO_NEW_PRIVS: u64 = 39;
        const PR_SET_THP_DISABLE: u64 = 41;
        const PR_GET_THP_DISABLE: u64 = 42;
        const PR_SET_VMA: u64 = 0x5356_4d41;
        Ok(match args[0] {
            PR_SET_NAME => {
                let mut name = [0u8; 16];
                ctx.read_mem(GuestAddr(args[1]), &mut name)?;
                // The kernel keeps fifteen bytes and a terminator, however long the name given.
                let end = name.iter().position(|b| *b == 0).unwrap_or(15).min(15);
                name[end..].fill(0);
                self.thread_names.insert(ctx.task(), name);
                0
            }
            PR_GET_NAME => {
                let name = self.thread_names.get(&ctx.task()).copied().unwrap_or_else(|| {
                    // Unnamed, a thread is called after its program, as the kernel calls it.
                    let mut name = [0u8; 16];
                    let program = self.program.as_deref().unwrap_or("guest");
                    let base = program.rsplit('/').next().unwrap_or(program).as_bytes();
                    let n = base.len().min(15);
                    name[..n].copy_from_slice(&base[..n]);
                    name
                });
                ctx.write_mem(GuestAddr(args[1]), &name)?;
                0
            }
            PR_GET_PDEATHSIG => {
                ctx.write_mem(GuestAddr(args[1]), &0i32.to_le_bytes())?;
                0
            }
            PR_GET_DUMPABLE | PR_GET_NO_NEW_PRIVS => 1,
            PR_GET_TIMERSLACK => 50_000,
            PR_GET_THP_DISABLE => 0,
            // Accepted with no effect.
            PR_SET_PDEATHSIG | PR_SET_DUMPABLE | PR_SET_TIMERSLACK | PR_SET_NO_NEW_PRIVS
            | PR_SET_THP_DISABLE | PR_SET_VMA => 0,
            _ => -errno::EINVAL,
        })
    }

    /// The `struct stat` for one object, as the guest's C library expects to read it.
    ///
    /// x86-64 layout; unwritten fields are zero. No inode means the random source, a character
    /// device.
    fn stat_of(&self, inode: Option<u64>, length: u64) -> [u8; STAT_BYTES] {
        let mode = self.mode_of(inode);
        let length = if mode == stat::DIRECTORY_MODE { 0 } else { length };

        let mut buf = [0u8; STAT_BYTES];
        put(&mut buf, 0, &stat::DEVICE.to_le_bytes());
        put(&mut buf, 8, &inode.unwrap_or(0).to_le_bytes());
        put(&mut buf, 16, &1u64.to_le_bytes());
        put(&mut buf, 24, &mode.to_le_bytes());
        put(&mut buf, 48, &length.to_le_bytes());
        put(&mut buf, 56, &stat::BLOCK_SIZE.to_le_bytes());
        put(&mut buf, 64, &length.div_ceil(512).to_le_bytes());
        // Times at the wall epoch.
        for at in [72, 88, 104] {
            put(&mut buf, at, &epoch_seconds().to_le_bytes());
        }
        buf
    }

    /// Answers `futex`, which is where a threaded guest does all of its waiting.
    ///
    /// Returns `None` when the calling task blocks; the caller turns that into a task that is not a
    /// scheduling candidate until a wake releases it.
    fn futex(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        args: [u64; 6],
    ) -> Result<(Option<i64>, Vec<Wake>), KernelError> {
        let word = args[0];
        let op = futex_op::base(args[1] as u32);
        let val = args[2] as u32;

        match op {
            futex_op::WAIT | futex_op::WAIT_BITSET => {
                let bits = if op == futex_op::WAIT_BITSET { args[5] as u32 } else { u32::MAX };
                // Re-read the word, since a waker may have changed it.
                if Self::read_word(ctx, word)? != val {
                    return Ok((Some(-errno::EAGAIN), Vec::new()));
                }
                // A pending signal ends the wait before it starts.
                if self.pending_signals.get(&ctx.task()).is_some_and(|q| !q.is_empty()) {
                    return Ok((Some(-errno::EINTR), Vec::new()));
                }
                // Timers are numbered per wait, so a stale one fires into nothing.
                let seq = self.wait_seq.get(&ctx.task()).map_or(1, |n| n + 1);
                self.wait_seq.insert(ctx.task(), seq);
                // A world alone arms the timer; a cluster arms it itself. WAIT is relative,
                // WAIT_BITSET absolute.
                if args[3] != 0 && !self.timeouts_armed {
                    let value = Self::read_timespec(ctx, GuestAddr(args[3]))?;
                    let wall = args[1] as u32 & futex_op::CLOCK_REALTIME != 0;
                    let ahead = if op == futex_op::WAIT_BITSET {
                        value.saturating_sub(Self::deadline_clock(ctx, wall).get())
                    } else {
                        value
                    };
                    let at = ctx.deadline_after(ahead);
                    self.timeouts.insert((at, ctx.task()), (word, seq));
                }
                self.waiters.entry(word).or_default().push(Waiter { task: ctx.task(), bits });
                Ok((None, Vec::new()))
            }

            futex_op::WAKE | futex_op::WAKE_BITSET => {
                let bits = if op == futex_op::WAKE_BITSET { args[5] as u32 } else { u32::MAX };
                let woken = self.release(ctx, word, val as usize, bits)?;
                let count = woken.len() as i64;
                Ok((Some(count), woken))
            }

            futex_op::REQUEUE | futex_op::CMP_REQUEUE => {
                // Wake `val` from the first word, move `val2` to the second; CMP checks `val3`.
                let val2 = usize::try_from(args[3]).unwrap_or(usize::MAX);
                let second = args[4];
                let expected = args[5] as u32;

                if op == futex_op::CMP_REQUEUE && Self::read_word(ctx, word)? != expected {
                    return Ok((Some(-errno::EAGAIN), Vec::new()));
                }
                let woken = self.release(ctx, word, val as usize, u32::MAX)?;
                let count = woken.len() as i64;

                // Requeued in order, without a draw.
                let moved: Vec<Waiter> = self.waiters.remove(&word).unwrap_or_default();
                let (move_now, stay) = moved.split_at(moved.len().min(val2));
                if !move_now.is_empty() {
                    self.waiters.entry(second).or_default().extend_from_slice(move_now);
                }
                if !stay.is_empty() {
                    self.waiters.insert(word, stay.to_vec());
                }
                Ok((Some(count), woken))
            }

            // A lock whose word names its holder.
            //
            // No priorities are modelled; the word holds its owner, waiters queue, and the lock is
            // handed over inside the call.
            futex_op::LOCK_PI | futex_op::TRYLOCK_PI => {
                let held = Self::read_word(ctx, word)?;
                if held & !futex_op::WAITERS == 0 {
                    Self::write_word(ctx, word, Self::thread_id(ctx.task()))?;
                    return Ok((Some(0), Vec::new()));
                }
                // Already held by the caller: `EDEADLK`, as the kernel answers.
                if held & !futex_op::WAITERS == Self::thread_id(ctx.task()) {
                    return Ok((Some(-errno::EDEADLK), Vec::new()));
                }
                if op == futex_op::TRYLOCK_PI {
                    return Ok((Some(-errno::EAGAIN), Vec::new()));
                }
                Self::write_word(ctx, word, held | futex_op::WAITERS)?;
                self.waiters
                    .entry(word)
                    .or_default()
                    .push(Waiter { task: ctx.task(), bits: u32::MAX });
                Ok((None, Vec::new()))
            }

            // Handed to the longest waiter, never dropped.
            futex_op::UNLOCK_PI => {
                let mut queue = self.waiters.remove(&word).unwrap_or_default();
                if queue.is_empty() {
                    Self::write_word(ctx, word, 0)?;
                    return Ok((Some(0), Vec::new()));
                }
                let next = queue.remove(0);
                let mut owner = Self::thread_id(next.task);
                if !queue.is_empty() {
                    owner |= futex_op::WAITERS;
                    self.waiters.insert(word, queue);
                }
                Self::write_word(ctx, word, owner)?;
                Ok((Some(0), vec![Wake { task: next.task, ret: 0 }]))
            }

            // An unmodelled operation, named on the timeline, since the guest's own error will
            // not say which.
            other => {
                ctx.emit(EventKind::Diagnostic {
                    code: "BOHR-2001".to_owned(),
                    message: format!(
                        "futex operation {other} is not modelled; see docs/PLATFORMS.md §4.2"
                    ),
                })?;
                Ok((Some(-errno::EINVAL), Vec::new()))
            }
        }
    }

    /// Whether `fd` names a regular file — the calls a disk model has an opinion about. The
    /// random source and the standard descriptors are not its business.
    pub(crate) fn is_file(&self, fd: i64) -> bool {
        self.open.get(&fd).is_some_and(|f| f.inode.is_some())
    }

    /// Whether a write through `fd` is durable by the time it returns, because it was asked for.
    pub(crate) fn writes_through(&self, fd: i64) -> bool {
        self.open.get(&fd).is_some_and(|f| f.durable)
    }

    /// Where the next positional-free transfer on `fd` lands.
    pub(crate) fn file_position(&self, fd: i64) -> u64 {
        self.open.get(&fd).map_or(0, |f| f.position)
    }

    /// Where a write through `fd` lands: at the end of the file for an appending descriptor,
    /// whatever offset was named, as Linux does for `pwrite` too; otherwise at the named offset,
    /// or at the descriptor's position.
    pub(crate) fn write_offset(&self, fd: i64, at: Option<u64>) -> u64 {
        let Some(file) = self.open.get(&fd) else {
            return at.unwrap_or(0);
        };
        match (file.appending, file.inode, at) {
            (true, Some(inode), _) => self.content.get(&inode).map_or(0, |held| held.len() as u64),
            (_, _, Some(at)) => at,
            _ => file.position,
        }
    }

    /// The content a descriptor refers to, for the disk model's crash bookkeeping.
    pub(crate) fn inode_of(&self, fd: i64) -> Option<u64> {
        self.open.get(&fd).and_then(|f| f.inode)
    }

    /// Every file on this disk, by name, with how long each is.
    ///
    /// By name, which reports need and which survives a restart.
    pub(crate) fn files(&self) -> Vec<(String, u64)> {
        self.names
            .iter()
            .filter(|(_, inode)| {
                !self.directories.contains(inode) && !self.synthetic.contains(inode)
            })
            .map(|(name, inode)| {
                (name.clone(), self.content.get(inode).map_or(0, |c| c.len()) as u64)
            })
            .collect()
    }

    /// Everything on this world's disk, as one value a crash model can hold on to.
    ///
    /// Generated `/proc` files are left out.
    pub(crate) fn disk_image(&self) -> DiskImage {
        #[cfg(feature = "chaos-proc-on-the-disk")]
        let off_the_disk = crate::chaos::kernel_answers_stay_off_the_disk();
        #[cfg(not(feature = "chaos-proc-on-the-disk"))]
        let off_the_disk = true;

        let mut content = self.content.clone();
        let mut names = self.names.clone();
        if off_the_disk && !self.synthetic.is_empty() {
            content.retain(|inode, _| !self.synthetic.contains(inode));
            names.retain(|_, inode| !self.synthetic.contains(inode));
        }

        DiskImage {
            content,
            names,
            links: self.links.clone(),
            directories: self.directories.clone(),
            next_inode: self.next_inode,
        }
    }

    /// Whether this file is one the kernel generates itself.
    pub(crate) fn is_synthetic(&self, inode: u64) -> bool {
        self.synthetic.contains(&inode)
    }

    /// Puts a file the kernel answers out of itself into this world, and answers its number.
    ///
    /// What `openat` does for `/proc`, callable directly for tests.
    #[cfg(test)]
    pub(crate) fn synthesise(&mut self, path: &str, content: Vec<u8>) -> u64 {
        let inode = self.next_inode;
        self.next_inode += 1;
        self.names.insert(path.to_owned(), inode);
        self.content.insert(inode, FileBytes::from_vec(content));
        self.synthetic.insert(inode);
        inode
    }

    /// Whether this world issued the descriptor and has not taken it back.
    pub(crate) fn holds_fd(&self, fd: i64) -> bool {
        self.open.contains_key(&fd)
    }

    /// One file's bytes, by the number that identifies it.
    ///
    /// Cloned, sharing blocks.
    pub(crate) fn file_content(&self, inode: u64) -> Option<&FileBytes> {
        self.content.get(&inode)
    }

    /// Puts one file back to held content, or removes it where there was none.
    ///
    /// One file only, as a failed sync affects only its file.
    pub(crate) fn restore_file(&mut self, inode: u64, content: Option<FileBytes>) {
        match content {
            Some(bytes) => {
                self.content.insert(inode, bytes);
            }
            None => {
                self.content.remove(&inode);
            }
        }
    }

    /// Puts the disk back to a held image. Descriptors are the caller's affair: a crash drops
    /// them with the process, which is why this does not touch the open table.
    ///
    /// Generated `/proc` files survive, since they are not in the image.
    pub(crate) fn restore_disk(&mut self, image: DiskImage) {
        let kept: Vec<(u64, FileBytes, String)> = self
            .synthetic
            .iter()
            .filter_map(|inode| {
                let content = self.content.get(inode)?.clone();
                let name = self.names.iter().find(|(_, at)| *at == inode)?.0.clone();
                Some((*inode, content, name))
            })
            .collect();

        self.content = image.content;
        self.names = image.names;
        self.links = image.links;
        self.directories = image.directories;
        // The image's numbering, raised past any surviving synthetic file.
        self.next_inode = image.next_inode;
        for (inode, content, name) in kept {
            self.next_inode = self.next_inode.max(inode + 1);
            self.content.insert(inode, content);
            self.names.insert(name, inode);
        }
    }

    /// What the guest is told the time is: the stop's node-local reading.
    ///
    /// The single choke point for presented time, which the shared-clock injection replaces.
    pub(crate) fn presented_now(ctx: &StopCtx<'_, '_>) -> Tick {
        #[cfg(feature = "chaos-node-clock-shared")]
        return crate::chaos::shared_now(ctx);
        #[cfg(not(feature = "chaos-node-clock-shared"))]
        ctx.now()
    }

    /// What the guest is told the wall time is: the same reading on top of the wall epoch until
    /// the node's clock is stepped, after which it can move back while [`Self::presented_now`]
    /// never does.
    pub(crate) fn presented_wall(ctx: &StopCtx<'_, '_>) -> Tick {
        #[cfg(feature = "chaos-node-clock-shared")]
        return crate::chaos::shared_now(ctx);
        #[cfg(not(feature = "chaos-node-clock-shared"))]
        ctx.wall_now()
    }

    /// Whether a `clock_gettime` clock id reads wall time.
    ///
    /// `CLOCK_REALTIME`, its coarse and alarm forms, and `CLOCK_TAI` follow the wall clock and are
    /// what `settimeofday` moves; every other id counts elapsed time and never goes back.
    pub(crate) const fn reads_wall(clock_id: u64) -> bool {
        matches!(clock_id, 0 | 5 | 8 | 11)
    }

    /// The clock an absolute deadline is measured against: the wall one where the caller named it.
    ///
    /// `pthread_cond_timedwait` and `clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME)` pass wall
    /// times.
    pub(crate) fn deadline_clock(ctx: &StopCtx<'_, '_>, wall: bool) -> Tick {
        if wall { Self::presented_wall(ctx) } else { Self::presented_now(ctx) }
    }

    /// Removes `task` from `word`'s queue. True when it was actually waiting; a timeout whose
    /// waiter was already woken fires into nothing.
    pub(crate) fn cancel_wait(&mut self, task: TaskId, word: u64) -> bool {
        let Some(queue) = self.waiters.get_mut(&word) else {
            return false;
        };
        let before = queue.len();
        queue.retain(|w| w.task != task);
        let removed = queue.len() < before;
        if queue.is_empty() {
            self.waiters.remove(&word);
        }
        removed
    }

    /// Releases up to `count` waiters on `word`, choosing each from the tape.
    #[cfg_attr(feature = "chaos-futex-order", allow(unused_variables))]
    fn release(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        word: u64,
        count: usize,
        bits: u32,
    ) -> Result<Vec<Wake>, KernelError> {
        let mut woken = Vec::new();
        // Reused across the whole wake.
        let mut eligible: Vec<usize> = Vec::new();
        while woken.len() < count {
            let Some(queue) = self.waiters.get_mut(&word) else {
                break;
            };
            eligible.clear();
            eligible.extend((0..queue.len()).filter(|i| queue[*i].bits & bits != 0));
            if eligible.is_empty() {
                break;
            }
            // The tape chooses the waiter.
            #[cfg(feature = "chaos-futex-order")]
            let pick = crate::chaos::first_waiter(eligible.len());
            #[cfg(not(feature = "chaos-futex-order"))]
            let pick = ctx.draw_index(eligible.len(), DrawSite::SyncWake)?;
            let waiter = queue.remove(eligible[pick]);
            if queue.is_empty() {
                self.waiters.remove(&word);
            }
            woken.push(Wake { task: waiter.task, ret: 0 });
        }
        Ok(woken)
    }

    /// Reads the 32-bit word a futex operation is predicated on.
    fn read_word(ctx: &mut StopCtx<'_, '_>, addr: u64) -> Result<u32, KernelError> {
        let mut buf = [0u8; 4];
        ctx.read_mem(GuestAddr(addr), &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn write_word(ctx: &mut StopCtx<'_, '_>, addr: u64, value: u32) -> Result<(), KernelError> {
        ctx.write_mem(GuestAddr(addr), &value.to_le_bytes())
    }

    /// What the guest is told its thread is called, which is what an owned lock word holds.
    ///
    /// The same number `gettid` answers, since callers compare it with the lock word.
    fn thread_id(task: TaskId) -> u32 {
        (i64::from(task.get()) + GUEST_PID) as u32
    }

    /// Releases a task parked in a futex wait this world holds, to take a signal: the wait ends
    /// with `EINTR`, and the signal rides on the task's next stop.
    fn interrupt_wait(&mut self, task: TaskId) -> Vec<Wake> {
        let mut released = Vec::new();
        for queue in self.waiters.values_mut() {
            let before = queue.len();
            queue.retain(|w| w.task != task);
            if queue.len() != before {
                released.push(Wake { task, ret: -errno::EINTR });
            }
        }
        self.waiters.retain(|_, queue| !queue.is_empty());
        released
    }

    /// The effects with the oldest signal waiting for this task attached, where the task carries
    /// on from this stop with an answer: the kernel delivers it as the task returns.
    fn with_pending_signal(&mut self, task: TaskId, mut effects: Effects) -> Effects {
        let TaskState::Runnable(Resume::Emulate { ret } | Resume::EmulateAndSignal { ret, .. }) =
            effects.state
        else {
            return effects;
        };
        let Some(queue) = self.pending_signals.get_mut(&task) else {
            return effects;
        };
        if queue.is_empty() {
            return effects;
        }
        let signal = queue.remove(0);
        if queue.is_empty() {
            self.pending_signals.remove(&task);
        }
        self.raised.entry(task).or_default().push(signal);
        effects.state = TaskState::Runnable(Resume::EmulateAndSignal { ret, signal });
        effects
    }

    /// Reads a `timespec` as a count of nanoseconds.
    pub(crate) fn read_timespec(
        ctx: &mut StopCtx<'_, '_>,
        addr: GuestAddr,
    ) -> Result<u64, KernelError> {
        let mut buf = [0u8; 16];
        ctx.read_mem(addr, &mut buf)?;
        let seconds = u64::from_le_bytes(buf[..8].try_into().unwrap_or_default());
        let nanos = u64::from_le_bytes(buf[8..].try_into().unwrap_or_default());
        Ok(seconds.saturating_mul(Tick::SECOND.get()).saturating_add(nanos))
    }

    /// Writes a `timespec` derived from virtual time, and reports what was written.
    fn write_timespec(
        ctx: &mut StopCtx<'_, '_>,
        addr: GuestAddr,
        presented: Tick,
    ) -> Result<[u8; 8], KernelError> {
        #[cfg(feature = "chaos-syscall-passthru")]
        let now = {
            let _ = presented;
            crate::chaos::host_nanos()
        };
        #[cfg(not(feature = "chaos-syscall-passthru"))]
        let now = presented.get();
        let mut buf = [0u8; 16];
        buf[..8].copy_from_slice(&(now / Tick::SECOND.get()).to_le_bytes());
        buf[8..].copy_from_slice(&(now % Tick::SECOND.get()).to_le_bytes());
        ctx.write_mem(addr, &buf)?;
        Ok(short_digest(&buf))
    }

    /// Writes a `timeval`, which is the same instant at microsecond resolution.
    fn write_timeval(ctx: &mut StopCtx<'_, '_>, addr: GuestAddr) -> Result<[u8; 8], KernelError> {
        let now = Self::presented_wall(ctx).get();
        let mut buf = [0u8; 16];
        buf[..8].copy_from_slice(&(now / Tick::SECOND.get()).to_le_bytes());
        let micros = (now % Tick::SECOND.get()) / Tick::MICROSECOND.get();
        buf[8..].copy_from_slice(&micros.to_le_bytes());
        ctx.write_mem(addr, &buf)?;
        Ok(short_digest(&buf))
    }

    fn syscall(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ctx: &mut StopCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        // Waiting builds its own effects.
        if nr == sys::FUTEX {
            return self.futex_effects(ctx, args);
        }

        // Mapping synchronisation happens here, once, and only for mapped files.
        let mapped = self.mapped_inode(nr, args);
        // The write's offset, taken before the descriptor moves.
        let wrote_from = mapped.and(self.open.get(&(args[0] as i64))).map_or(0, |f| f.position);
        if let Some(inode) = mapped
            && matches!(nr, sys::READ | sys::PREAD64)
            && self.is_mapped_writable(inode)
        {
            self.collect_mappings(ctx, inode)?;
        }

        // The host creates the thread; the model records the word it clears on exit.
        if nr == sys::CLONE || nr == sys::CLONE3 {
            let (flags, word) = if nr == sys::CLONE {
                (args[0], args[3])
            } else {
                let mut flags = [0u8; 8];
                let mut word = [0u8; 8];
                let Some(at) = offset(GuestAddr(args[0]), CLONE_ARGS_CHILD_TID) else {
                    return Ok(Effects::go(Resume::Emulate { ret: -errno::EFAULT }, SYSCALL_COST));
                };
                ctx.read_mem(GuestAddr(args[0]), &mut flags)?;
                ctx.read_mem(at, &mut word)?;
                (u64::from_le_bytes(flags), u64::from_le_bytes(word))
            };
            // A task in namespaces of its own would see a machine the world does not answer for.
            if flags & clone_flags::NEW_NAMESPACES != 0 {
                return Err(ctx.refuse(
                    "BOHR-2001",
                    format_args!(
                        "a task in new namespaces is not modelled (clone flags {flags:#x}); see                          docs/PLATFORMS.md §4.2"
                    ),
                ));
            }
            if word != 0 {
                self.creating.insert(ctx.task(), word);
            }
            ctx.emit(EventKind::Syscall { nr, args, ret: 0, emulated: false, data_digest: None })?;
            // An untraced task would run outside the schedule, so the flag is cleared before the
            // host sees the call: in the register for `clone`, in the guest's arguments for
            // `clone3`.
            if flags & clone_flags::UNTRACED != 0 {
                let kept = flags & !clone_flags::UNTRACED;
                if nr == sys::CLONE {
                    let mut regs = ctx.regs()?;
                    regs.args[0] = kept;
                    ctx.set_regs(&regs)?;
                } else {
                    ctx.write_mem(GuestAddr(args[0]), &kept.to_le_bytes())?;
                }
            }
            return Ok(Effects::go(Resume::Continue, SYSCALL_COST));
        }

        let answer = match route(nr) {
            Some(Subsystem::Vfs) => self.answer_vfs(nr, args, ctx)?,
            Some(Subsystem::Clock) => self.answer_clock(nr, args, ctx)?,
            Some(Subsystem::Process) => self.answer_process(nr, args, ctx)?,
            Some(Subsystem::Signals) => self.answer_signals(nr, args, ctx)?,
            Some(Subsystem::IoUring) => self.answer_io_uring(nr, args, ctx)?,
            Some(Subsystem::Mappings) => self.answer_mappings(nr, args, ctx)?,
            None => {
                let ret = self.unmodelled(ctx, nr, args)?;
                Answer::Reply((ret, Resume::Emulate { ret }, None))
            }
        };
        let (ret, resume, data_digest) = match answer {
            Answer::Reply(reply) => reply,
            Answer::Done(effects) => return Ok(effects),
        };

        if let Some(inode) = mapped {
            let changed = match nr {
                sys::WRITE | sys::WRITEV if ret > 0 => wrote_from..wrote_from + ret as u64,
                sys::PWRITE64 | sys::PWRITEV if ret > 0 => args[3]..args[3] + ret as u64,
                // A length change refreshes the whole window.
                sys::FTRUNCATE => 0..u64::MAX,
                _ => 0..0,
            };
            if !changed.is_empty() {
                self.refresh_mappings(ctx, inode, changed)?;
            }
        }

        ctx.emit(EventKind::Syscall { nr, args, ret, emulated: true, data_digest })?;
        Ok(Effects::go(resume, SYSCALL_COST))
    }

    /// The file a call names, where something has that file mapped.
    ///
    /// `None` when nothing is mapped, at the cost of one lookup.
    fn mapped_inode(&self, nr: u32, args: [u64; 6]) -> Option<u64> {
        if self.mappings.is_empty() {
            return None;
        }
        if !matches!(
            nr,
            sys::READ
                | sys::PREAD64
                | sys::WRITE
                | sys::WRITEV
                | sys::PWRITE64
                | sys::PWRITEV
                | sys::FTRUNCATE
        ) {
            return None;
        }
        let inode = self.open.get(&(args[0] as i64))?.inode?;
        self.mappings.values().any(|m| m.inode == inode).then_some(inode)
    }

    /// The number this run gave a synchronisation word, assigning one where it is new.
    ///
    /// In first-touch order ([`bohr_types::SyncWordId`]).
    fn sync_word(&mut self, word: GuestAddr) -> SyncWordId {
        let next = self.sync_words.len() as u32;
        *self.sync_words.entry(word).or_insert(SyncWordId(next))
    }

    /// Turns a `futex` call into effects, recording the waiting and the waking as events.
    fn futex_effects(
        &mut self,
        ctx: &mut StopCtx<'_, '_>,
        args: [u64; 6],
    ) -> Result<Effects, KernelError> {
        let word = GuestAddr(args[0]);
        let named = self.sync_word(word);
        let (ret, wake) = self.futex(ctx, args)?;

        ctx.emit(EventKind::Syscall {
            nr: sys::FUTEX,
            args,
            ret: ret.unwrap_or(0),
            emulated: true,
            data_digest: None,
        })?;
        for released in &wake {
            ctx.emit(EventKind::Woke { task: released.task, on: named })?;
        }

        Ok(match ret {
            Some(ret) => Effects::go(Resume::Emulate { ret }, SYSCALL_COST).releasing(wake),
            None => {
                ctx.emit(EventKind::Waited { on: named })?;
                Effects::wait(SYSCALL_COST).releasing(wake)
            }
        })
    }
}

/// Reads a NUL-terminated path out of the guest, or reports that no path of this model's could be
/// read there — because it ran past what a path may be, or because it ran off the address space.
///
/// Read in large steps, narrowed on failure, so most paths cost one crossing.
impl MinimalWorld {
    /// The absolute name a path the guest gave stands for.
    ///
    /// Names are flat, so a relative one is made absolute against the working directory.
    fn absolute(&self, path: &str) -> String {
        if path.starts_with('/') {
            return path.to_owned();
        }
        format!("{}/{path}", self.cwd.trim_end_matches('/'))
    }

    /// Where a relative name is taken from.
    ///
    /// `AT_FDCWD` means the working directory; another number names an open directory, as RocksDB
    /// and `std::fs` use.
    fn under(&self, dirfd: i64, name: &str) -> Result<String, i64> {
        if name.starts_with('/') {
            return Ok(name.to_owned());
        }
        if dirfd == AT_FDCWD {
            return Ok(self.absolute(name));
        }
        match self.dir_of.get(&dirfd) {
            Some(dir) if name.is_empty() => Ok(dir.clone()),
            Some(dir) => Ok(format!("{}/{name}", dir.trim_end_matches('/'))),
            // `ENOTDIR` for a non-directory, `EBADF` for an unheld descriptor.
            None if self.open.contains_key(&dirfd) => Err(-errno::ENOTDIR),
            None => Err(-errno::EBADF),
        }
    }

    /// The name a path ends at, following symbolic links at the last component.
    ///
    /// Only for calls that follow links; `unlink`, `rename` and `readlink` act on the link. A
    /// relative target resolves against the link's directory.
    fn resolve(&self, path: &str) -> String {
        // Stops sooner than Linux's 40, ending on a link that answers `ENOENT`.
        const HOPS: usize = 8;
        let mut at = path.to_owned();
        for _ in 0..HOPS {
            let Some(target) = self.links.get(&at) else {
                return at;
            };
            at = if target.starts_with('/') {
                target.clone()
            } else {
                let dir = at.rsplit_once('/').map_or("", |(dir, _)| dir);
                format!("{dir}/{target}")
            };
        }
        at
    }
}

/// What reading a path argument produced.
///
/// A null pointer is answered `EFAULT`, which `glibc` probes `statx` and `newfstatat` with. Another
/// unreadable address ends the run, since it may be the supervisor's fault.
pub(crate) enum PathArg {
    /// The name the guest gave.
    Named(String),
    /// The guest gave no name at all.
    Unreadable,
    /// Longer than a path may be.
    TooLong,
}

impl PathArg {
    /// What a syscall answers when the argument was not a name.
    fn errno(&self) -> i64 {
        match self {
            Self::Named(_) => 0,
            Self::Unreadable => -errno::EFAULT,
            Self::TooLong => -errno::ENAMETOOLONG,
        }
    }
}

pub(crate) fn read_c_string(
    ctx: &mut StopCtx<'_, '_>,
    addr: GuestAddr,
) -> Result<PathArg, KernelError> {
    // Down to one byte, for paths at the very end of a mapping.
    const STEPS: [usize; 4] = [256, 32, 8, 1];

    if addr.0 == 0 {
        return Ok(PathArg::Unreadable);
    }

    let mut out = Vec::new();
    let mut chunk = [0u8; STEPS[0]];
    while out.len() < PATH_MAX {
        let Some(at) = offset(addr, out.len() as u64) else {
            return Ok(PathArg::TooLong);
        };
        let read = STEPS
            .iter()
            .copied()
            .find(|step| ctx.read_mem(at, &mut chunk[..*step]).is_ok())
            .ok_or_else(|| ctx.refuse("BOHR-2002", format_args!("unreadable path at {at}")))?;

        if let Some(end) = chunk[..read].iter().position(|b| *b == 0) {
            out.extend_from_slice(&chunk[..end]);
            return Ok(PathArg::Named(String::from_utf8_lossy(&out).into_owned()));
        }
        out.extend_from_slice(&chunk[..read]);
    }
    Ok(PathArg::TooLong)
}

impl WorldModel for MinimalWorld {
    /// The earliest timed wait this world armed itself.
    fn next_deadline(&self) -> Option<Tick> {
        self.timeouts.keys().next().map(|(at, _)| *at)
    }

    /// Ends every timed wait whose deadline has come with `ETIMEDOUT`.
    ///
    /// Only into the wait that armed it, checked at firing.
    fn on_deadline(&mut self, ctx: &mut DeadlineCtx<'_, '_>) -> Result<DueEffects, KernelError> {
        let now = ctx.world_now();
        let mut effects = DueEffects::default();
        while let Some(entry) = self.timeouts.first_entry() {
            if entry.key().0 > now {
                break;
            }
            let ((_, task), (word, seq)) = entry.remove_entry();
            if self.wait_seq.get(&task) == Some(&seq) && self.cancel_wait(task, word) {
                effects.wake.push(Wake { task, ret: -errno::ETIMEDOUT });
            }
        }
        Ok(effects)
    }

    fn identity(&self) -> WorldModelId {
        if self.surveying.is_some() {
            WorldModelId::new("minimal-survey", 3)
        } else {
            WorldModelId::new("minimal", 3)
        }
    }

    fn task_created(&mut self, task: TaskId, parent: TaskId) {
        self.born.insert(task);
        if let Some(word) = self.creating.remove(&parent) {
            self.exit_word.insert(task, word);
        }
    }

    fn task_exited(&mut self, task: TaskId) -> Vec<Wake> {
        self.creating.remove(&task);
        self.ended.insert(task);
        self.pending_signals.remove(&task);
        self.raised.remove(&task);

        // A task that ended while queued is removed.
        self.waiters.retain(|_, queue| {
            queue.retain(|w| w.task != task);
            !queue.is_empty()
        });

        let Some(word) = self.exit_word.remove(&task) else {
            return Vec::new();
        };
        // A thread's exit wakes every waiter on its word at once, without a draw.
        self.waiters
            .remove(&word)
            .unwrap_or_default()
            .into_iter()
            .map(|w| Wake { task: w.task, ret: 0 })
            .collect()
    }

    fn handle(&mut self, stop: &Stop, ctx: &mut StopCtx<'_, '_>) -> Result<Effects, KernelError> {
        // A node's first task is registered at its first stop.
        self.born.insert(ctx.task());
        match &stop.kind {
            // `exit_group` ends every task, including parked ones that make no stop.
            StopKind::SyscallEntry { nr, args } if *nr == sys::EXIT_GROUP => {
                let effects = self.syscall(*nr, *args, ctx)?;
                let ended = NodeOp::Ended { node: ctx.node(), by: ctx.task() };
                Ok(Effects { node_ops: vec![ended], ..effects })
            }
            StopKind::SyscallEntry { nr, args } => {
                let effects = self.syscall(*nr, *args, ctx)?;
                Ok(self.with_pending_signal(ctx.task(), effects))
            }

            // A replaced mapping's address is known only now.
            StopKind::SyscallExit { ret } => self.mapping_placed(ctx, *ret),

            // `cpuid` from the committed table.
            StopKind::CpuIdentification { leaf, subleaf } => {
                let [eax, ebx, ecx, edx] = crate::cpu::identify(*leaf, *subleaf);
                let mut regs = ctx.regs()?;
                regs.rax = u64::from(eax);
                regs.rbx = u64::from(ebx);
                regs.rcx = u64::from(ecx);
                regs.rdx = u64::from(edx);
                ctx.set_regs(&regs)?;
                ctx.emit(EventKind::Diagnostic {
                    code: "BOHR-2007".to_owned(),
                    message: format!("processor identification leaf {leaf:#x}/{subleaf} answered"),
                })?;
                Ok(Effects::go(Resume::Continue, SYSCALL_COST))
            }

            // `rdtsc` reads the virtual clock.
            StopKind::TimestampCounter { .. } => {
                let now = Self::presented_now(ctx);
                let mut regs = ctx.regs()?;
                regs.rax = now.get() & 0xFFFF_FFFF;
                regs.rdx = now.get() >> 32;
                ctx.set_regs(&regs)?;
                ctx.emit(EventKind::ClockRead { presented: now })?;
                Ok(Effects::go(Resume::Continue, SYSCALL_COST))
            }

            // A signal this world sent is let through.
            StopKind::Signal { signal }
                if self.raised.get(&ctx.task()).is_some_and(|sent| sent.contains(signal)) =>
            {
                if let Some(sent) = self.raised.get_mut(&ctx.task()) {
                    if let Some(at) = sent.iter().position(|s| s == signal) {
                        sent.remove(at);
                    }
                    if sent.is_empty() {
                        self.raised.remove(&ctx.task());
                    }
                }
                Ok(Effects::go(Resume::DeliverSignal { signal: *signal }, Tick::ZERO))
            }

            // A fatal signal ends the task; continuing would re-fault forever.
            StopKind::Signal { signal } if FATAL_SIGNALS.contains(signal) => {
                ctx.emit(EventKind::Diagnostic {
                    code: "BOHR-3005".to_owned(),
                    message: format!("signal {signal} ended {}", ctx.task()),
                })?;
                Ok(Effects::go(Resume::Terminate { signal: Some(*signal) }, Tick::ZERO))
            }

            StopKind::Signal { signal } => {
                ctx.emit(EventKind::Diagnostic {
                    code: "BOHR-3005".to_owned(),
                    message: format!("signal {signal} withheld from {}", ctx.task()),
                })?;
                Ok(Effects::go(Resume::Continue, Tick::ZERO))
            }

            other => {
                Err(ctx.refuse("BOHR-5001", format_args!("this model does not handle {other:?}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every call the world answers is one the syscall filter stops the guest at, and each is
    /// answered by one subsystem: an admitted call never reaches the world, so an answer for one
    /// would be code no traced run takes.
    #[test]
    fn every_call_answered_is_traced_and_routed_once() {
        let mut seen = BTreeSet::new();
        for (_, handles) in ROUTES {
            for nr in handles {
                assert!(seen.insert(*nr), "{nr} is routed to two subsystems");
            }
        }
        let untraced: Vec<(u32, Option<&str>)> = answered()
            .filter(|nr| bohr_trace::syscall_traced(*nr) != Some(true))
            .map(|nr| (nr, bohr_trace::syscall_name(nr)))
            .collect();
        assert!(untraced.is_empty(), "answered and not traced: {untraced:?}");
    }
}
