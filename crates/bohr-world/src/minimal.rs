use std::collections::{BTreeMap, BTreeSet};

use bohr_backend::{Resume, Stop, StopKind};
use bohr_kernel::{
    DueEffects, Effects, FaultCatalog, KernelError, NodeOp, TaskState, Wake, WorldCtx, WorldModel,
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
/// Content, length and position are the model's own, which is what makes a run over a database
/// reproduce: a real file carries a length, timestamps and an inode from the host, and a guest that
/// branched on any of them would branch differently on another machine.
#[derive(Clone, Copy, Debug, Default)]
struct OpenFile {
    /// The content this descriptor refers to. Absent for the endless random source.
    ///
    /// A number rather than a name, because a descriptor outlives the name: a program that opens a
    /// file and unlinks it immediately still reads and writes it, and that is how temporary files
    /// are made.
    inode: Option<u64>,
    /// Where the next positional-free read or write lands.
    position: u64,
    /// Whether a write through this descriptor is durable when it returns.
    ///
    /// Set by `O_DSYNC` and `O_SYNC` at open. A store in its most careful mode uses one of these
    /// rather than calling `fsync` after every write — the durability is asked for once, at open,
    /// and never appears in the call stream again. A model that read only the calls would see a
    /// store writing and never syncing, and would take every one of those writes away at the next
    /// crash: not a defect found, a promise the model never heard being made.
    durable: bool,
    /// Whether every write through this descriptor lands at the end of the file.
    ///
    /// Set by `O_APPEND` at open, and read where a write chooses its offset: the position a
    /// descriptor carries is not where an appending write goes.
    appending: bool,
}

/// Size of the `struct sysinfo` a caller reads back.
///
/// Zero-filled and no more: what it would report is host state, and every field of it is a number
/// this world has no honest answer for. Filling it is still necessary -- answering success over a
/// structure nobody wrote hands the guest its own stack.
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
/// Only the two that make a descriptor. The rest ask about flags that are all true here — nothing
/// is inherited across an exec and nothing blocks — so they answer zero, and zero is a wrong answer
/// for exactly these two.
mod fcntl_cmd {
    /// Lowest free descriptor at or above the third argument, onto the same open file.
    pub(crate) const DUPFD: u32 = 0;
    /// The same, closed on exec, which changes nothing this model does.
    pub(crate) const DUPFD_CLOEXEC: u32 = 1030;
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
    /// Release one, handing it to whoever is queued rather than dropping it.
    pub(crate) const UNLOCK_PI: u32 = 7;
    /// `LOCK_PI` that reports rather than waits.
    pub(crate) const TRYLOCK_PI: u32 = 8;

    /// The bit a held word carries while somebody is queued behind it.
    pub(crate) const WAITERS: u32 = 0x8000_0000;

    /// Set when the word is private to one process, which changes nothing this model does.
    pub(crate) const PRIVATE_FLAG: u32 = 128;
    /// Set when an absolute timeout is a wall-clock time rather than a monotonic one.
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
/// **Which way the truth runs depends on what the mapping was asked for**, and the two cases are
/// not variations on one another.
///
/// A mapping that only reads is the world's to keep truthful: the bytes are copied in when it is
/// made and again whenever the file changes under it, and nothing the guest does can disagree with
/// the file. That is how a store reaches its pages, and it is exact.
///
/// A mapping that writes is the other way round. The guest stores to memory and no call announces
/// it, so the mapping — not the world's copy — is where the file's current contents live. The world
/// takes them back at the two moments anyone can tell the difference: when the file is read through
/// a descriptor, and when it is made durable. Deliberately *not* at a crash, because a crash keeps
/// what was synced, and copying back there would make every unsynced write durable and every
/// durability finding a lie.
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
    /// A mapping outlives the descriptor that made it — closing one is the ordinary next line
    /// after mapping — so this is a label and never a lookup. What the work is done against is
    /// the inode.
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
    /// Which region of a submission ring this is, where it is one rather than a file.
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
pub(crate) fn refuse_submission(ctx: &mut WorldCtx<'_, '_>, what: &str) -> KernelError {
    ctx.refuse(
        "BOHR-2004",
        format_args!("io_uring {what} is not modelled; see docs/PLATFORMS.md §4.1"),
    )
}

/// What a submission returned, read off the effects of the plain call that carried it out.
///
/// **A submission that would wait, or pass to the host, is refused.** Every completion is posted
/// before `io_uring_enter` returns, so a call that parks its task has no moment to complete at,
/// and one the host carries out did work this world did not see.
pub(crate) fn submission_answer(
    ctx: &mut WorldCtx<'_, '_>,
    effects: &Effects,
) -> Result<i64, KernelError> {
    match effects.state {
        bohr_kernel::TaskState::Runnable(
            Resume::Emulate { ret } | Resume::EmulateAndSignal { ret, .. },
        ) => Ok(ret),
        _ => Err(refuse_submission(ctx, "a submission that waits or reaches the host")),
    }
}

/// The one descriptor number this model never hands out.
///
/// The guest already holds a descriptor at this number that means something else: the coverage
/// channel the supervisor opened and passed down (`docs/API.md` §5.5). World descriptors count
/// upward from three and would reach it on a guest holding a couple of hundred files — a
/// blockchain client holds more than that — and the guest would then have one number standing for
/// two things. It is skipped rather than moved because the number is the interceptor's to choose
/// and this model is above that boundary; what this model owes is not to collide with it.
const RESERVED_FD: i64 = 200;

/// Longest path this model will resolve. Beyond it a name is refused rather than truncated, so a
/// long path can never be mistaken for a short one that exists.
const PATH_MAX: usize = 4096;

/// Largest transfer answered in one call. A guest asking for more is told how much it got, which is
/// a short read every caller of `read` already handles.
const MAX_TRANSFER: usize = 1 << 20;

/// Virtual time a syscall costs. Uniform, because a per-syscall cost model without measurements
/// behind it would be invented precision.
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

/// The directory descriptor an `*at` call carries, read at the width the kernel reads it.
///
/// A descriptor is an `int`, and `AT_FDCWD` is a negative one: handed over in a sixty-four bit
/// register it arrives as `0x00000000ffffff9c`, and taken as a whole register that is four
/// thousand million rather than minus one hundred. Every relative name then resolved against a
/// directory nobody had opened, so `open` -- which `glibc` spells `openat(AT_FDCWD, ...)` --
/// answered `EBADF` for every program on the system.
fn dirfd_of(raw: u64) -> i64 {
    i64::from(raw as i32)
}

/// `AT_FDCWD`: the call names its own working directory rather than a directory it has open.
const AT_FDCWD: i64 = -100;

/// `AT_EMPTY_PATH`: the call is about the descriptor rather than about a name under it.
const AT_EMPTY_PATH: u64 = 0x1000;

/// The one directory that exists before a guest makes any, and the only path with no parent.
const ROOT: &str = "/";
/// Its number. Guest files are numbered after it and after the directories below.
const ROOT_INODE: u64 = 1;

/// What a guest is told when it asks the kernel about itself.
///
/// **Generated, fixed, and small.** A program that wants to know when it started, or whether the
/// process holding a lock file is itself, reads it here — and a store refusing to open because it
/// could not find its own process is a store this engine cannot run, for a reason that has nothing
/// to do with storage. What is offered is the shape of the answer rather than a copy of the host's:
/// every number is this model's own, so two machines produce the same bytes.
///
/// The set is grown from what real targets are seen reading and no faster. Anything else under
/// `/proc` is absent, which is behaviour a portable program already handles.
fn generated(path: &str) -> Option<Vec<u8>> {
    // The twenty-second field is when the process started, which is the one field anything here is
    // actually after; the rest are the shape a parser walks to reach it. Zero throughout, because a
    // figure invented for a field nobody reads is a figure somebody will one day read.
    let stat = format!(
        "{GUEST_PID} (guest) S 0 {GUEST_PID} {GUEST_PID} 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 \
         100 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n"
    );
    let own = format!("/proc/{GUEST_PID}/stat");
    match path {
        "/proc/self/stat" => Some(stat.into_bytes()),
        p if p == own => Some(stat.into_bytes()),
        // Seconds since the world began, and seconds spent idle. Virtual time is the run's, and a
        // fixed pair is what makes two runs agree.
        "/proc/uptime" => Some(b"100.00 100.00\n".to_vec()),
        _ => None,
    }
}

/// The directories a guest may assume are already there.
///
/// Every real filesystem a program is ever started on has these, so a program writing to `/tmp`
/// without making it first is not making a mistake — it is doing the ordinary thing. A world that
/// refused it would refuse an open that succeeds everywhere, and a correct program would look
/// broken for a reason that is entirely this model's.
///
/// The list is the hierarchy standard's own, in the order it is written here so that numbering is a
/// function of the list rather than of anything a run does. Parents come before what sits under
/// them. They are empty and stay empty: nothing here has contents until a guest puts something in.
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

/// `clock_nanosleep` flag asking for a deadline rather than a duration.
pub(crate) const TIMER_ABSTIME: u64 = 1;

/// Signals that end a task rather than being handled by it.
const FATAL_SIGNALS: [u32; 5] = [4, 6, 8, 9, 11];

/// Size of `struct pollfd`.
const POLLFD_BYTES: usize = 8;

/// Most descriptors a single `poll` is answered for.
const MAX_POLL_ENTRIES: usize = 1024;

/// Size of one `utsname` field, including the terminator.
const UTSNAME_FIELD: usize = 65;

/// Size of `struct utsname`: six fields of the size above.
const UTSNAME_BYTES: usize = UTSNAME_FIELD * 6;

/// Kernel release the guest is told it runs on. Fixed, because guests branch on it and the answer
/// has to be the same on every machine a bundle replays on.
const PRESENTED_RELEASE: &str = "5.15.0-bohr";

/// Kernel version string, fixed for the same reason.
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
/// A limit is a fact about a host, and a guest that sized a table or chose a strategy from the real
/// one would behave differently on a machine with a different `ulimit`. Descriptors get the number
/// a stock Linux gives; everything else is unlimited, which is the answer that makes a guest do the
/// most rather than the least.
const fn limit_for(resource: u64) -> (u64, u64) {
    if resource == RLIMIT_NOFILE { (1024, 1_048_576) } else { (u64::MAX, u64::MAX) }
}

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
    /// `EXT4_SUPER_MAGIC`. The disk model is written to ext4's `data=ordered` rules, so this is
    /// what a guest asking what it is standing on should be told; naming anything else would send
    /// one that branches on the answer down a path this model does not implement.
    pub(crate) const MAGIC: u64 = 0xEF53;
    /// Blocks the filesystem is said to hold, at [`super::stat::BLOCK_SIZE`] each: sixteen
    /// gibibytes, far past what fits in supervisor memory and so never the binding limit.
    pub(crate) const BLOCKS: u64 = 4 * 1024 * 1024;
    /// Longest name component, which is every real Linux filesystem's answer.
    pub(crate) const NAME_MAX: u64 = 255;
}

/// The number the first file created is given. A real one comes from the host filesystem and would
/// differ between machines.
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
/// Checked before the pieces are walked, because the answer is the whole call rather than a shorter
/// one: taking the first thousand and reporting their length told the caller a write had succeeded
/// that had left the rest of its data behind. Linux answers `EINVAL`.
pub(crate) const fn too_many_iovecs(count: u64) -> bool {
    count > MAX_IOVECS
}

/// The largest a stored file may become.
///
/// A file here is a buffer in the supervisor's own memory, so a guest that seeks far past the end
/// and writes one byte is asking the supervisor to allocate the gap. The bound turns that into an
/// answer the guest already knows how to handle, rather than an allocation nobody can serve.
const MAX_FILE_BYTES: usize = 1 << 26;

/// Where a write ends, or nothing where it would take the file past what may be held.
fn grown_end(at: usize, len: usize) -> Option<usize> {
    at.checked_add(len).filter(|end| *end <= MAX_FILE_BYTES)
}

/// A guest address at an offset from another, or nothing where the sum leaves the address space.
///
/// Every walk over a guest-supplied array goes through here. An address that wraps is not an
/// address the guest could have meant, and arriving at it by arithmetic rather than refusing it is
/// how a supervisor ends up reading somewhere nobody named.
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
/// **File bytes are shared, not copied.** An image is taken at every completed `fsync`, and a store
/// syncing a file it has grown to megabytes would otherwise copy every other file on the disk each
/// time — a cost of disk size times sync count, per run, per worker. Sharing makes an image cost
/// one counted reference per block of each file, and a write copies only the blocks it lands in.
#[derive(Clone, Debug)]
pub(crate) struct DiskImage {
    pub(crate) content: BTreeMap<u64, FileBytes>,
    pub(crate) names: BTreeMap<String, u64>,
    /// Which of those names are directories, held with them because a restore that rolled the
    /// namespace back and left the set alone would leave a name that is a file and a directory at
    /// once — or neither.
    pub(crate) directories: BTreeSet<u64>,
    pub(crate) next_inode: u64,
}

/// Where a guest's own output is delivered.
///
/// Supplied by the caller rather than opened here, so this crate performs no host I/O of its own
/// and a test can read back exactly what the guest wrote.
pub type OutputSink = Box<dyn std::io::Write + Send>;

/// Answers clock, randomness and output, and refuses everything else.
pub struct MinimalWorld {
    catalog: FaultCatalog,
    /// Synchronisation words this run has touched, in the order it first touched them.
    ///
    /// Kept so a trace can name a word without naming its address; see [`Self::sync_word`].
    sync_words: BTreeMap<GuestAddr, SyncWordId>,
    output: Option<OutputSink>,
    /// Descriptors the guest holds, and what each refers to.
    open: BTreeMap<i64, OpenFile>,
    /// File contents, by the number that identifies them. Nothing reaches the host filesystem: a
    /// run has to start from the same state every time, and a file left over from the last run is
    /// state.
    ///
    /// Shared block by block with every disk image taken of this world, so a write copies a block
    /// only while a snapshot still holds the bytes it is about to change.
    content: BTreeMap<u64, FileBytes>,
    /// Names, each pointing at content. Unlinking removes the name and leaves the content, which
    /// is what an open descriptor still refers to.
    names: BTreeMap<String, u64>,
    /// Files this world answers out of itself rather than off any disk.
    ///
    /// **What the kernel says about a process is not on the platter, and a crash cannot take it
    /// away.** They are held here with everything else so a reader seeks and reads them like any
    /// file, and they are kept out of every disk image so that no snapshot holds one, no restore
    /// rolls one back and no tear shortens one. A guest that asked the system about itself and was
    /// told it does not exist would be right to be surprised: a real one is answered whatever the
    /// disk has just been through.
    synthetic: BTreeSet<u64>,
    /// Live mappings of files, by where each landed.
    mappings: BTreeMap<u64, Mapping>,
    /// What each task's replaced `mmap` was for, until it returns and says where it went.
    mapping: BTreeMap<TaskId, PendingMap>,
    /// Which of those names are directories.
    ///
    /// **A directory here is a name that can be made, asked about and opened — not a container.**
    /// Resolution is still by the whole path and never by walking components, so there are no
    /// links, no `..` and no working directory to be relative to. What the set buys is the one
    /// thing a flat namespace cannot say: that a name exists, is not a file, and is the parent
    /// some other name needs before it can be created. A store that keeps its data and its lock
    /// beside each other in a directory it made itself needs exactly that and nothing more.
    directories: BTreeSet<u64>,
    /// Whether the guest is shown a kernel with `io_uring` (`[linux] io_uring = "ring"`).
    io_uring: bool,
    /// Permission bits a guest set with the `chmod` family, by inode; the rest read as made.
    ///
    /// Held beside the content rather than in it, so a crash image keeps a file's bytes and a
    /// restarted node sees the bits a file was made with.
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
    /// Once per run rather than once per call: a guest that probes in a loop would otherwise fill
    /// its own timeline with one sentence.
    declined_ring: bool,
    /// The directory behind each descriptor a guest opened one through.
    ///
    /// Kept separately from the descriptor itself because a descriptor is held by value and a path
    /// is not, and because only the calls that take a directory ever ask.
    dir_of: BTreeMap<i64, String>,
    /// The program this world was started with, as `/proc/self/exe` reports it.
    ///
    /// Configuration rather than a fact about the host: it is the argument the run was given, and
    /// a replay is given the same one, so the answer is the same on another machine.
    program: Option<String>,
    /// Names that stand for other names, and the text each was created with.
    ///
    /// The text rather than the name it resolves to, because that is what `readlink` answers and
    /// what a relative target means: a link made as `validator.log -> validator-9.log` follows to
    /// whatever sits beside the link at the moment it is followed, not to whatever sat there when
    /// it was made. Keeping the resolved name would freeze the first answer and make the link a
    /// copy of a decision rather than a link.
    links: BTreeMap<String, String>,
    /// The number the next file created is given.
    next_inode: u64,
    /// Tasks waiting on each futex word, keyed by guest address.
    ///
    /// The queue is the model's own, which is the point: the kernel is free to wake waiters in any
    /// order, and a run that inherited that order would not reproduce.
    waiters: BTreeMap<u64, Vec<Waiter>>,
    /// The word each task clears when it ends, read from the call that created it.
    exit_word: BTreeMap<TaskId, u64>,
    /// The word named by the most recent creation call in each task, waiting for the task it
    /// produced to be announced.
    creating: BTreeMap<TaskId, u64>,
    /// Signals sent to a task by another, waiting for that task's next stop.
    pending_signals: BTreeMap<TaskId, Vec<u32>>,
    /// Signals this world has had the kernel raise on a task, which come back as a stop to be let
    /// through rather than withheld.
    raised: BTreeMap<TaskId, Vec<u32>>,
    /// Tasks this world has seen begin, and those it has seen end, to tell a thread from a number.
    born: BTreeSet<TaskId>,
    ended: BTreeSet<TaskId>,
    /// Reused across transfers so a guest reading in a loop does not allocate per call.
    transfer: Vec<u8>,
    /// Everything the guest wrote to its standard output, where a projection asks for it.
    ///
    /// Kept only when something is going to read it: an ordinary run has no reason to hold a copy
    /// of what it already delivered.
    stdout: Option<Vec<u8>>,
    /// What `getcwd` answers.
    ///
    /// A fact about the filesystem the guest was given, so it travels with the state rather than
    /// with the process.
    cwd: String,
    /// Whether something outside this world arms the timeout a wait carries.
    timeouts_armed: bool,
    /// The calls this build cannot model, and how often each was asked for.
    ///
    /// `Some` only under a survey. An ordinary run refuses the first one and never sees a second,
    /// which is the point of §5.7 and also why the whole list needs a mode of its own to collect.
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
        // The guest's own process, as a name under `/proc`. It has to be there before anybody
        // looks: a library that finds a process by reading the directory finds nothing if the
        // entry only appears once somebody has opened what is inside it, and a program asking
        // whether it is itself would conclude that it is not running.
        world.add_directory(&format!("/proc/{GUEST_PID}"));
        world
    }

    /// The world with nothing on it but the root, which is what the standard directories are added
    /// to. Separate so the list above is applied in one place and cannot be half-applied.
    fn bare() -> Self {
        Self {
            catalog: FaultCatalog::default(),
            output: None,
            sync_words: BTreeMap::new(),
            // The three a process is born holding. Present as descriptors and not only as places
            // to write, because a program is entitled to ask about them: a Go runtime checks that
            // nought, one and two are open before it will start at all, and one told they are not
            // opens `/dev/null` over them and gives up when there is none. What it had been told
            // was untrue — this world had them all along and answered questions about them as
            // though it had never issued them.
            open: (0..=2).map(|fd| (fd, OpenFile::default())).collect(),
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
            cwd: "/".to_owned(),
            surveying: None,
            timeouts_armed: false,
        }
    }

    /// The same world, with somebody outside it arming the timeouts a wait can carry.
    ///
    /// **Set by the cluster model for each node it holds, and false for a world standing alone.**
    /// Timers belong to the cluster: it arms a `FutexTimeout` around the wait this world parks.
    /// Alone, there is nothing to make one fire, so a wait with a timeout that nothing wakes is a
    /// wait for ever -- which the kernel then calls a deadlock, a finding invented against a
    /// correct program. Refusing loudly is §5.7; the flag is what tells the two situations apart
    /// rather than guessing from the arguments.
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

    /// Adds bytes the guest wrote out to what a projection will read.
    ///
    /// **Public because a cluster answers those writes itself.** Five processes share one terminal
    /// and the order they reach it is the schedule, so the distributed world intercepts writes to
    /// the first two descriptors before delegating to the node -- which meant the buffer a
    /// projection reads was never filled, `project(Stdout)` handed back nothing for every
    /// implementation, the digests agreed and a differential run reported `Agree` for a comparison
    /// that had not happened. That is the failure 5.20 exists to forbid, so the interception hands
    /// what it took to the node it took it from.
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
    /// Applied before anything runs, so a node's durability baseline — captured when its disk is
    /// first looked at — is the prepared state rather than an empty disk. That is what makes a
    /// crash restore to "the run began here" instead of to nothing.
    #[must_use]
    pub fn from_state(mut self, image: &crate::StateImage) -> Self {
        for (path, content) in image.files() {
            // The directories a file sits under are made first. An image walked from a host tree
            // carries nested paths, and a world holding the file without the directory over it
            // would answer `ENOENT` to the very `stat` a guest makes to check that the state it
            // was handed is there.
            self.make_parents(path);
            let inode = self.next_inode;
            self.next_inode += 1;
            self.content.insert(inode, FileBytes::from_vec(content.clone()));
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
    /// One kernel has both, and a ring's entries are carried by the cluster that routes them, which
    /// is why only a cluster turns this on.
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
    /// Its identity changes with it, and that is the lock rather than a convention: every answer
    /// a survey gives is invented, so a bundle claiming this world is refused by
    /// `Manifest::check_provenance` and no survey can ever be mistaken for a run.
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
    /// An ordinary run refuses and ends — §5.7, because a call that reached the host
    /// unnoticed is a hole nothing downstream would find. A survey answers `ENOSYS` and records
    /// the number, because what a survey is for is the whole list rather than its first entry.
    ///
    /// Both spellings of the refusal live here so they cannot drift apart: a ring is refused by
    /// its own name because a reader told "unsupported syscall 425" goes looking for a table entry
    /// to add, and there is none — the ring submits work without a syscall per operation.
    fn unmodelled(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
        // Once per number, not once per call: a guest polling an unmodelled call in a loop would
        // otherwise fill its own timeline with the same sentence.
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
    /// **A polled queue is accepted with its poller asleep.** The interface says what happens when
    /// the kernel's thread is not running, and every caller of a polled ring implements it: the
    /// queue's flag word says the poller must be woken, and waking it is `io_uring_enter`, which
    /// is the call this world answers.
    fn open_ring(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
        args: [u64; 6],
    ) -> Result<i64, KernelError> {
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
        let fd =
            self.attach(OpenFile { inode: None, position: 0, durable: false, appending: false });
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
    /// Never more entries than the guest has completion slots free, which keeps
    /// `IORING_FEAT_NODROP` true without an overflow list; a full queue is answered `EBUSY`. An
    /// index naming no slot is counted as dropped and ends the batch, as it does in a kernel.
    pub(crate) fn take_submissions(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
        // A polled queue is drained whole when its poller wakes: the kernel ignores the count the
        // caller passes, and liburing passes nought.
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
        ctx: &mut WorldCtx<'_, '_>,
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
    fn read_u32(ctx: &mut WorldCtx<'_, '_>, at: Option<u64>) -> Result<u32, KernelError> {
        let Some(at) = at else {
            return Ok(0);
        };
        let mut bytes = [0u8; 4];
        ctx.read_mem(GuestAddr(at), &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Answers the call that opens a ring, and says so on the timeline.
    ///
    /// Declined rather than refused, and the difference is not a softening. The refusal existed
    /// because a ring bypasses per-call interception: work is submitted through shared memory and
    /// the supervisor sees none of it, which is a hole in determinism nothing downstream would
    /// find. Answering "this kernel has no io_uring" closes the same hole at the door -- no ring
    /// is created, so there is nothing to submit through and every operation goes back through
    /// the calls this model does answer. Entering or registering a ring stays refused, because
    /// reaching either without one is a state this model cannot account for.
    ///
    /// It is also a world a guest meets in production: a kernel built without the option, or an
    /// operator who turned it off. What it costs is that the guest takes its fallback path, which
    /// is not the path it takes where the ring is there -- so the fact goes on the timeline of
    /// every run rather than only of a survey, and a report made from that run carries it.
    fn decline_ring(&mut self, ctx: &mut WorldCtx<'_, '_>) -> Result<i64, KernelError> {
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
    /// A handful of calls are deliberately told to be unimplemented so a caller takes the fallback
    /// path it already keeps for an older kernel. From the guest's side that is the same wall as a
    /// call nothing models at all, and a survey mentioning only the second kind under-reports twice
    /// over: it lets a program look surveyed clean when it stopped dead, and it sends a reader off
    /// to add a table row for a call the table already has.
    ///
    /// So a survey counts these as well, and says which kind each was. An ordinary run answers them
    /// exactly as it did before, silently, because the fallback is the point.
    fn unimplemented(&mut self, ctx: &mut WorldCtx<'_, '_>, nr: u32) -> Result<i64, KernelError> {
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
    /// The count reported to the guest is the count it asked for, whatever the sink did with the
    /// bytes: whether anyone is still reading the other end is a property of the observer, and a
    /// run whose outcome depended on it would not reproduce.
    fn write_out(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
        // Inline rather than through `keep_written_out`: the two fields are disjoint, so no copy
        // of the transfer buffer is needed on a path every write takes.
        if let Some(kept) = self.stdout.as_mut() {
            let room = crate::MAX_PROJECTION_BYTES.saturating_sub(kept.len());
            kept.extend_from_slice(&self.transfer[..room.min(self.transfer.len())]);
        }
        Ok((len as i64, Some(short_digest(&self.transfer))))
    }

    /// Joins the segments a gathering write names into one buffer.
    ///
    /// Segments past the first megabyte are dropped rather than answered short: a caller that
    /// hands over more than the transfer bound in one call is told how much arrived, and every
    /// caller of a write already handles a short answer.
    /// How many bytes a vector of segments asks for, without copying any of them.
    ///
    /// A scattering call carries its count of segments where a plain one carries its length, so
    /// anything that narrates a length has to work it out rather than read it off the register.
    pub(crate) fn iov_bytes(
        ctx: &mut WorldCtx<'_, '_>,
        iov: GuestAddr,
        count: u64,
    ) -> Result<u32, KernelError> {
        let mut total: u64 = 0;
        for i in 0..count.min(MAX_IOVECS) {
            let mut entry = [0u8; IOVEC_BYTES];
            let Some(at) = offset(iov, i * IOVEC_BYTES as u64) else {
                break;
            };
            ctx.read_mem(at, &mut entry)?;
            total =
                total.saturating_add(u64::from_le_bytes(entry[8..].try_into().unwrap_or_default()));
        }
        Ok(total.min(MAX_TRANSFER as u64) as u32)
    }

    pub(crate) fn gather(
        ctx: &mut WorldCtx<'_, '_>,
        iov: GuestAddr,
        count: u64,
    ) -> Result<Vec<u8>, KernelError> {
        let mut joined = Vec::new();
        for i in 0..count.min(MAX_IOVECS) {
            let mut entry = [0u8; IOVEC_BYTES];
            let Some(at) = offset(iov, i * IOVEC_BYTES as u64) else {
                break;
            };
            ctx.read_mem(at, &mut entry)?;
            let base = u64::from_le_bytes(entry[..8].try_into().unwrap_or_default());
            let len = u64::from_le_bytes(entry[8..].try_into().unwrap_or_default()) as usize;
            let take = len.min(MAX_TRANSFER.saturating_sub(joined.len()));
            if take == 0 {
                break;
            }
            let start = joined.len();
            joined.resize(start + take, 0);
            ctx.read_mem(GuestAddr(base), &mut joined[start..])?;
        }
        Ok(joined)
    }

    /// Writes bytes already gathered, at a named offset or wherever the descriptor stands.
    ///
    /// A named offset leaves the descriptor where it was, which is the whole reason a caller uses
    /// one: two threads writing different parts of one file share a descriptor and not a position.
    fn write_bytes(&mut self, fd: i64, bytes: &[u8], at: Option<u64>) -> (i64, Option<[u8; 8]>) {
        // A standard descriptor is only standard until something is duplicated onto it. The three
        // of them are in the table from the start and refer to no file, so the question is not
        // whether the number is open but whether it now stands for one: a program that sends its
        // own output to a log opens the log and puts it at 1, and answering by the number alone
        // sent every one of those writes to the console, where nothing the run stores shows them.
        if self.file_behind(fd).is_none() && (fd == 1 || fd == 2) {
            if let Some(sink) = self.output.as_mut() {
                let _ = sink.write_all(bytes);
                let _ = sink.flush();
            }
            // The same buffer `write_out` fills. A vectored write to a standard descriptor came
            // through here and skipped it, so a projection over standard output saw one kind of
            // write and not the other.
            self.keep_written_out(bytes);
            return (bytes.len() as i64, Some(short_digest(bytes)));
        }
        let Some(file) = self.open.get(&fd).copied() else {
            return (-errno::EBADF, None);
        };
        let Some(inode) = file.inode else {
            return (bytes.len() as i64, Some(short_digest(bytes)));
        };
        // An appending descriptor writes at the end of the file as it is now, which is not where
        // its own position stands: two of them on one file both append rather than overwrite.
        let start = match at {
            Some(at) => at as usize,
            None if file.appending => self.content.get(&inode).map_or(0, |held| held.len()),
            None => file.position as usize,
        };
        if grown_end(start, bytes.len()).is_none() {
            return (-errno::EFBIG, None);
        }
        self.content.entry(inode).or_default().write_at(start, bytes);
        if at.is_none()
            && let Some(open) = self.open.get_mut(&fd)
        {
            open.position += bytes.len() as u64;
        }
        (bytes.len() as i64, Some(short_digest(bytes)))
    }

    /// Opens a path, creating it where the flags ask for that.
    fn open_at(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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

        // What the kernel says about the process is made on the way in and then is a file like any
        // other to read — but never one to any disk, which is what `synthetic` marks it as.
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
                    // **The name is taken, and the caller said it must not be.** Succeeding anyway
                    // meant a lock-file protocol never saw the contention it was written against,
                    // so the very race it guards was unreachable in this world.
                    if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
                        return Ok(-errno::EEXIST);
                    }
                    // A directory opens for reading, which is how a store makes the name it just
                    // created durable, and refuses everything that would write through it.
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
                    // A file cannot be made where there is nowhere to put it. Enforced rather than
                    // waved through, because a guest that skipped its `mkdir` would otherwise open
                    // a file the real filesystem would have refused it, and go on to be judged on
                    // a path it could never have taken.
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
            // **Appending is where a write goes, not a detail of how it got there.** Ignored, a
            // store reopening its log with `O_APPEND|O_WRONLY` wrote from nought and overwrote the
            // head of the state it had been given -- silently wrong, in the one domain this crate
            // exists to model.
            appending: flags & O_APPEND != 0,
        });
        if inode.is_some_and(|inode| self.directories.contains(&inode)) {
            self.note_directory(fd, &path);
        }
        Ok(fd)
    }

    /// Answers `getdents64`: what a directory holds, in as much of it as the buffer takes.
    ///
    /// **The descriptor's position is the index of the next entry**, which is what makes the second
    /// call carry on and the third report the end. A caller reads until it is told nothing, so a
    /// listing that started again would never finish.
    ///
    /// Order is the namespace's own, which is sorted, so two runs list a directory the same way. A
    /// real kernel promises no order at all and hands back whatever its own structures held; taking
    /// that freedom here would mean a run that reproduces only until a hash seed moves.
    fn list_directory(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
                // A buffer too small for even one entry is the caller's mistake and is told so;
                // one that filled is simply asked again.
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
        ctx: &mut WorldCtx<'_, '_>,
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
    /// The parent is the text before the last separator, because resolution here is by whole path:
    /// there is no walk to perform and so no component to be wrong about.
    fn parent_exists(&self, path: &str) -> bool {
        let Some(parent) = parent_of(path) else {
            return true;
        };
        self.names.get(parent).is_some_and(|inode| self.directories.contains(inode))
    }

    /// Replaces a mapping of a file with one of nothing, and remembers what it was for.
    ///
    /// The guest gets real memory from the host, which is the one thing this model cannot make on
    /// its own — an address space belongs to the process, and the only way to grow one is to let it
    /// ask. What the world supplies is the contents, written in when the call comes back.
    ///
    /// **It is made writable whatever was asked for**, because the supervisor's writes go through
    /// the same page protections the guest's do, and a mapping it could not write is one it could
    /// not keep truthful. The cost is stated rather than hidden: a guest that writes through a
    /// mapping it asked to be read-only is not stopped here, where the real kernel would fault it.
    /// That is a defect this model does not catch, and never a defect it invents.
    fn map_file(&mut self, ctx: &mut WorldCtx<'_, '_>, args: [u64; 6]) -> Option<Resume> {
        const PROT_WRITE: u64 = 0x2;
        const PROT_READ_WRITE: u64 = 0x1 | 0x2;
        const MAP_SHARED: u64 = 0x1;
        const MAP_PRIVATE: u64 = 0x2;
        const MAP_ANONYMOUS: u64 = 0x20;

        const PROT_EXEC: u64 = 0x4;

        // **A ring is not a file.** What the guest asks for here is a window onto the queue it was
        // given, named by the offset rather than by any content: the interface hands out three of
        // them and a caller reads where each one is from the block it was handed at setup.
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
        // **Executability is the guest's to ask for and not ours to drop.** Rewriting every file
        // mapping to read-write took it away, so a guest that maps code -- `dlopen`, a runtime
        // paging in a shared object -- faulted on the first instruction fetched from it and the
        // run reported a crashed guest: a finding manufactured by the model. Writability is still
        // forced, for the reason above; that is a defect this model fails to catch rather than one
        // it invents.
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
        ctx: &mut WorldCtx<'_, '_>,
        ret: i64,
    ) -> Result<Effects, KernelError> {
        let Some(pending) = self.mapping.remove(&ctx.task()) else {
            return Ok(Effects::go(Resume::Continue, Tick::ZERO));
        };
        // A mapping the host refused is not one to remember. The guest already has the error.
        if ret < 0 {
            return Ok(Effects::go(Resume::Continue, Tick::ZERO));
        }
        // A ring's region carries no file. What it carries is a control block the guest reads to
        // learn the shape of what it was given, and an anonymous mapping arrives as zeroes -- a
        // mask and a count of nought, which puts every submission in the same slot.
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
    /// Short where the file is: the rest of the mapping is the zeroes the host already put there,
    /// which is what a mapping past the end of a file reads as.
    fn show_range(
        &self,
        ctx: &mut WorldCtx<'_, '_>,
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
    /// **The range and not the whole window.** A store reserves a window far larger than the file
    /// behind it and writes a page at a time into it; copying the window on every write would move
    /// megabytes per call and turn a campaign against a real store into a campaign against this
    /// loop. Whole-file is asked for by passing the whole file.
    fn refresh_mappings(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
    /// The other direction, and the only one that needs asking for: a store to mapped memory makes
    /// no syscall, so the world learns of it here or not at all.
    fn collect_mappings(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
        inode: u64,
    ) -> Result<(), KernelError> {
        let written: Vec<Mapping> =
            self.mappings.values().filter(|m| m.writable && m.inode == inode).copied().collect();
        for mapping in written {
            // Only as far as the file goes. A window is routinely opened far past the end of what
            // it shows — a store reserves gigabytes and grows into them — and the bytes past that
            // end are not the file's: writing there is what the real thing answers with `SIGBUS`,
            // so reading them back would invent content from pages nobody wrote.
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
    /// `Err` where the range covers written mappings of more than one file. A sync is one operation
    /// with one fate, and a model that made several of them would draw a different number of times
    /// depending on how many mappings happened to be live — which is state deciding tape layout,
    /// the one thing the entropy contract forbids (`docs/API.md` §2.4). The shape is vanishingly
    /// rare and refusing it is honest; guessing at it would not be.
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
        ctx: &mut WorldCtx<'_, '_>,
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
    fn unmap(&mut self, ctx: &mut WorldCtx<'_, '_>, at: u64, len: u64) -> Result<(), KernelError> {
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
    /// Written into the refusal because those terms are the whole question. A mapping that only
    /// reads can be kept truthful — the bytes are copied in and copied again whenever the file
    /// changes under it. One that writes cannot: the guest stores to memory and no call says so, so
    /// the world would hold a file its own guest had already moved past.
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
    /// Used for a channel the world offers rather than one a guest makes: the guest is told a path
    /// and opens it, and a guest is not the one who should have to create the directory over a path
    /// it did not choose.
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
    /// Programs build descriptor sets and redirect by number on the assumption that a fresh
    /// descriptor is the smallest available one.
    fn attach(&mut self, file: OpenFile) -> i64 {
        let mut fd = FIRST_FREE_FD;
        while self.open.contains_key(&fd) || fd == RESERVED_FD {
            fd += 1;
        }
        // **Four allocators hand out descriptors from four bases, and the ranges were a comment.**
        // Files climb from three, sockets from 4096, epoll from 8192, event counters from 12288;
        // enough open files and this one walked into the socket range, where one number would have
        // had two owners and a write to a file would have reached a socket. Four thousand and
        // ninety-three files open at once is not a shape a scenario here reaches, so the assert is
        // the cheapest way to make the range a checked invariant rather than a paragraph.
        assert!(
            fd < crate::distributed::net::SOCKET_FD_BASE,
            "the file descriptors have run into the range the sockets are numbered from"
        );
        self.open.insert(fd, file);
        fd
    }

    /// Remembers where a descriptor opened on a directory sits.
    ///
    /// Only directories, because only they can be named by a later call. Keeping the path of every
    /// open file would be a second copy of the name table that nothing reads.
    fn note_directory(&mut self, fd: i64, path: &str) {
        self.dir_of.insert(fd, path.to_owned());
    }

    /// Reads into the guest, either from a stored file or from the endless random source.
    fn read_in(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
        fd: i64,
        addr: GuestAddr,
        len: usize,
        at: Option<u64>,
    ) -> Result<(i64, Option<[u8; 8]>), KernelError> {
        // The standard streams carry nothing in this world, which a reader sees as end of file.
        // Named rather than left to the table: they are open, and an open descriptor with no file
        // behind it is the endless random source — which is the one thing standard output must not
        // turn into when somebody reads it.
        if (0..=2).contains(&fd) {
            return Ok((0, None));
        }
        let Some(file) = self.open.get(&fd).copied() else {
            return Ok((-errno::EBADF, None));
        };
        let len = len.min(MAX_TRANSFER);

        let Some(inode) = file.inode else {
            // The random source is endless and has no position: every read is fresh tape.
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
        ctx: &mut WorldCtx<'_, '_>,
        fd: i64,
        addr: GuestAddr,
        len: usize,
        at: Option<u64>,
    ) -> Result<(i64, Option<[u8; 8]>), KernelError> {
        // Refused before the guest's memory is read, because a descriptor nobody holds is not a
        // write to gather bytes for.
        if !self.open.contains_key(&fd) {
            return Ok((-errno::EBADF, None));
        }

        let len = len.min(MAX_TRANSFER);
        self.transfer.clear();
        self.transfer.resize(len, 0);
        ctx.read_mem(addr, &mut self.transfer)?;

        // Growing the file, moving the position and choosing where an appending write lands are
        // done once, in `write_bytes`: two implementations of one rule diverge (rule 7), and the
        // appending case is where they would.
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
        0
    }

    /// Moves a stored file to another name, replacing whatever was there.
    fn rename(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
        dirs: (i64, i64),
        from: GuestAddr,
        to: GuestAddr,
        flags: u64,
    ) -> Result<i64, KernelError> {
        /// `RENAME_NOREPLACE`: fail rather than take a name that is already there.
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
        // A flag this model does not implement is refused rather than ignored: a caller that asked
        // for an exchange and got a replacement was answered a different operation than it made.
        if flags & !(NOREPLACE | EXCHANGE) != 0 {
            return Ok(-errno::EINVAL);
        }
        let Some(inode) = self.names.get(&from).copied() else {
            return Ok(-errno::ENOENT);
        };
        // **Atomic rename is the second pillar of a crash-safe store after `fsync`**, so what this
        // refuses matters as much as what it does. A name cannot be made where there is nowhere to
        // put it, by the same rule `openat` enforces.
        if !self.parent_exists(&to) {
            return Ok(-errno::ENOENT);
        }
        let held = self.names.get(&to).copied();
        if flags & EXCHANGE != 0 {
            let Some(other) = held else {
                return Ok(-errno::ENOENT);
            };
            self.names.insert(from, other);
            self.names.insert(to, inode);
            return Ok(0);
        }
        // The caller said the name must be free, and it is not.
        if held.is_some() && flags & NOREPLACE != 0 {
            return Ok(-errno::EEXIST);
        }
        // Names are flat strings here, so a directory's children are named by their own full
        // paths and would be stranded under the old one. Refused rather than half-done: a
        // directory that moved and left its contents behind is a filesystem no kernel produces.
        if self.directories.contains(&inode) {
            let prefix = format!("{from}/");
            if self.names.keys().any(|name| name.starts_with(&prefix)) {
                return Ok(-errno::ENOTEMPTY);
            }
        }
        self.names.remove(&from);
        self.names.insert(to, inode);
        // What the destination pointed at has lost its last name, so its content goes with it.
        // Left behind, it stayed in memory for the rest of the run.
        if let Some(replaced) = held.filter(|replaced| *replaced != inode) {
            self.directories.remove(&replaced);
            self.collect(replaced);
        }
        Ok(0)
    }

    /// Reports that none of the descriptors a caller asked about are ready, and none are invalid.
    /// Writes each entry's `revents` and says how many are ready, or the error the array earned.
    fn poll_ready(
        &self,
        ctx: &mut WorldCtx<'_, '_>,
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
            // The whole entry has to lie before the end of the address space, not only its start.
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
        ctx: &mut WorldCtx<'_, '_>,
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
    fn write_uname(ctx: &mut WorldCtx<'_, '_>, addr: GuestAddr) -> Result<i64, KernelError> {
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
    fn write_affinity(ctx: &mut WorldCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
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
    /// The rule a filesystem follows: a file exists while something can reach it. Keeping it beyond
    /// that would make a guest that writes temporary files grow the model for the whole run.
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
        // **A position the answer cannot carry is refused, and the descriptor does not move.** The
        // cast handed back a negative number, which every caller reads as an error while the
        // descriptor had already been left somewhere the caller was never told about.
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
        ctx: &mut WorldCtx<'_, '_>,
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
    /// The three standard numbers are in the table from the start and refer to nothing, so a
    /// descriptor being present says less than it looks like it says.
    fn file_behind(&self, fd: i64) -> Option<u64> {
        self.open.get(&fd).and_then(|file| file.inode)
    }

    /// Answers `dup`, `dup2` and `dup3`: a second number for a file already open.
    ///
    /// What this does not do: share a position. Linux gives the two numbers one file description,
    /// so a read through either advances both; here the second number gets a copy of the first,
    /// and the two walk the file independently. What that costs is a program that reads through
    /// one and expects the other to have moved -- a shell pipeline splitting a stream between two
    /// readers. What it buys is every program that duplicates a descriptor to put a log where its
    /// output goes and never reads through either, which is what the calls are almost always for.
    /// Sharing needs one description behind many numbers, and every descriptor in this model is
    /// held by value.
    fn duplicate(&mut self, from: i64, onto: Option<i64>, exact: bool) -> i64 {
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
        // `dup2` onto the number a descriptor already has is the one case that does nothing at
        // all, and `dup3` calls the same thing an error: the flag it carries would otherwise be
        // silently dropped on a call that changed nothing.
        if from == onto {
            return if exact { -errno::EINVAL } else { onto };
        }
        self.open.insert(onto, file);
        // The duplicate names the same directory, and takes over from whatever the number named
        // before: a number that stood for one directory and is now a copy of another would
        // resolve every later relative name under the wrong one.
        match self.dir_of.get(&from).cloned() {
            Some(dir) => self.dir_of.insert(onto, dir),
            None => self.dir_of.remove(&onto),
        };
        onto
    }

    /// Answers `readlink`: the text a name stands for.
    ///
    /// Four answers, and the distinction between them is what callers branch on. A name that
    /// stands for another gives its text; a name that exists and stands for nothing is `EINVAL`,
    /// which is how a caller learns it has a file rather than a link; a name that is not there is
    /// `ENOENT`. The two spellings of this call answered differently before -- `readlink` said
    /// `EINVAL` and `readlinkat` said `ENOENT`, for every path either was given -- so a program
    /// learnt one thing or the other about the same name depending on which it happened to use.
    ///
    /// `/proc/self/exe` is answered because a program asking where it is is asking an ordinary
    /// question, and the answer is the program this run was configured with rather than anything
    /// read off the host: a replay reconstructs the same argument list, so the bytes are the same
    /// on another machine.
    ///
    /// The buffer is filled to at most what the caller offered and the count is what was written,
    /// never a terminator: `readlink` does not write one, and a caller that trusted a terminator
    /// this model added would read past its own string everywhere else.
    fn read_link(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
                // Absolute, and absolute against this model rather than against the host. Linux
                // never answers this one with a relative name, and `glibc` does not merely cope
                // badly with one -- a static program reading a relative path here prints and
                // calls `abort` before reaching `main`. Resolving it against the working
                // directory the world carries keeps the answer the same on any machine, which
                // resolving it against the host's would not.
                match self.program.clone() {
                    Some(program) => self.absolute(&program),
                    None => return Ok(-errno::ENOENT),
                }
            }
            None if self.names.contains_key(&path) => return Ok(-errno::EINVAL),
            None => return Ok(-errno::ENOENT),
        };

        // A caller offering no room is asking for nothing, and Linux calls that a bad argument
        // rather than an empty answer.
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
    /// The text is kept as it was given rather than as it resolves. A store rotates a log by
    /// pointing one stable name at a fresh file beside it, and a link recorded as the name it
    /// happened to resolve to would go on naming yesterday's file after the rotation.
    ///
    /// What this does not do: check that the target is there. Linux does not either -- a link to
    /// nothing is an ordinary thing to make and an error only when it is followed -- and every
    /// caller here answers a name that resolves to nothing with `ENOENT` already.
    fn make_link(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
        // An empty target is not a name, and a link holding one would resolve to the directory it
        // sits in for ever after.
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
    /// A caller that gets nothing here has nowhere else to go — unlike the calls this model
    /// declines on purpose, `stat` has no older spelling to fall back to, and a program that
    /// cannot ask how big a file is stops.
    fn stat_at(&self, ctx: &mut WorldCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
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
    /// The mask asked for is not consulted; every basic field is filled and the mask handed back
    /// says so, which is what a kernel does for a filesystem that has them all.
    fn statx_at(&self, ctx: &mut WorldCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
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
        ctx: &mut WorldCtx<'_, '_>,
        dirfd: u64,
        name_at: u64,
        flags: u64,
    ) -> Result<Result<(Option<u64>, u64), i64>, KernelError> {
        let fd = dirfd_of(dirfd);
        let name = match read_c_string(ctx, GuestAddr(name_at))? {
            PathArg::Named(name) => name,
            other => return Ok(Err(other.errno())),
        };
        // The empty name is the descriptor itself, and asking where it sits would answer about
        // the directory it is under instead.
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
    /// One device, so the answer does not depend on which file was named — the path or descriptor
    /// is read only to refuse one that names nothing.
    ///
    /// Free space is derived from what the world actually holds, so a guest that sizes a mapping
    /// or a batch by it is sized by something true. It is not, however, what a full disk is made
    /// of here: `ENOSPC` is drawn (`docs/API.md` §5.2), and a drawn one can fire with room left in
    /// these numbers, exactly as a real one fires on quota or on a reservation `statfs` cannot see.
    fn write_statfs(
        &self,
        ctx: &mut WorldCtx<'_, '_>,
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
    /// Each is answered as an unprivileged process on an ordinary kernel is; one that would hand
    /// the guest a capability this world does not model — its own syscall filter, its memory map's
    /// layout — is refused `EINVAL`, the answer for an option a kernel does not have.
    fn prctl(&mut self, ctx: &mut WorldCtx<'_, '_>, args: [u64; 6]) -> Result<i64, KernelError> {
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
            // Accepted and of no effect: who is told of a parent's death, whether a core may be
            // written, how late a timer may fire, a name for a mapping. None of it is anything this
            // world's answers depend on.
            PR_SET_PDEATHSIG | PR_SET_DUMPABLE | PR_SET_TIMERSLACK | PR_SET_NO_NEW_PRIVS
            | PR_SET_THP_DISABLE | PR_SET_VMA => 0,
            _ => -errno::EINVAL,
        })
    }

    /// The `struct stat` for one object, as the guest's C library expects to read it.
    ///
    /// Field offsets are those of `struct stat` on x86-64. Everything not written stays zero,
    /// which is a valid value for every field a guest reads. No inode at all is the endless random
    /// source, which is a character device and has no length.
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
        buf
    }

    /// Answers `futex`, which is where a threaded guest does all of its waiting.
    ///
    /// Returns `None` when the calling task blocks; the caller turns that into a task that is not a
    /// scheduling candidate until a wake releases it.
    fn futex(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
        args: [u64; 6],
    ) -> Result<(Option<i64>, Vec<Wake>), KernelError> {
        let word = args[0];
        let op = futex_op::base(args[1] as u32);
        let val = args[2] as u32;

        match op {
            futex_op::WAIT | futex_op::WAIT_BITSET => {
                let bits = if op == futex_op::WAIT_BITSET { args[5] as u32 } else { u32::MAX };
                // The word is re-read here rather than trusted from the call: between the guest
                // deciding to wait and this point it may have been changed by the task that would
                // have woken it, and waiting then would be waiting forever.
                if Self::read_word(ctx, word)? != val {
                    return Ok((Some(-errno::EAGAIN), Vec::new()));
                }
                // A signal already waiting for this task ends the wait before it begins, and
                // before any timer is armed for a wait that never happened.
                if self.pending_signals.get(&ctx.task()).is_some_and(|q| !q.is_empty()) {
                    return Ok((Some(-errno::EINTR), Vec::new()));
                }
                // **A timer ends the wait that armed it and no later one.** It is not taken back
                // when the wait ends some other way; it is numbered, and a timer whose number is
                // not the task's current wait fires into nothing.
                let seq = self.wait_seq.get(&ctx.task()).map_or(1, |n| n + 1);
                self.wait_seq.insert(ctx.task(), seq);
                // A timed wait alone arms a timer here; in a cluster the cluster arms it, around
                // this call. WAIT carries a duration, WAIT_BITSET an absolute deadline on the
                // clock its flags name.
                if args[3] != 0 && !self.timeouts_armed {
                    let value = Self::read_timespec(ctx, GuestAddr(args[3]))?;
                    let wall = args[1] as u32 & futex_op::CLOCK_REALTIME != 0;
                    let ahead = if op == futex_op::WAIT_BITSET {
                        value.saturating_sub(Self::deadline_clock(ctx, wall).get())
                    } else {
                        value
                    };
                    let at = Tick(ctx.world_now().get().saturating_add(ahead));
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
                // The call takes two words and two counts: `val` waiters are released from the
                // first, `val2` of the rest move to the second, and the conditional form checks the
                // first word against `val3` before doing either.
                let val2 = usize::try_from(args[3]).unwrap_or(usize::MAX);
                let second = args[4];
                let expected = args[5] as u32;

                if op == futex_op::CMP_REQUEUE && Self::read_word(ctx, word)? != expected {
                    return Ok((Some(-errno::EAGAIN), Vec::new()));
                }
                let woken = self.release(ctx, word, val as usize, u32::MAX)?;
                let count = woken.len() as i64;

                // Whoever is left moves to the second word, keeping their order: requeuing is not
                // a wake and must not consume a draw.
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

            // A lock whose word names its holder rather than counting its waiters.
            //
            // Priority inheritance is what the kernel calls this family and none of it is modelled:
            // there are no priorities here, and one task runs at a time. What *is* modelled is the
            // part a guest can observe — that the word holds an owner, that a second thread queues
            // behind it, and that the lock passes from one to the next inside the call. A store
            // that keeps a mutex of this kind in a file it maps needs exactly that and nothing
            // about scheduling.
            futex_op::LOCK_PI | futex_op::TRYLOCK_PI => {
                let held = Self::read_word(ctx, word)?;
                if held & !futex_op::WAITERS == 0 {
                    Self::write_word(ctx, word, Self::thread_id(ctx.task()))?;
                    return Ok((Some(0), Vec::new()));
                }
                // **Held by the caller already.** Queueing it behind itself was a wait nothing
                // could end; the kernel says so instead, and a store that relocks a mutex it holds
                // is told what it did rather than left to hang.
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

            // The lock is handed over rather than dropped, which is the whole difference from a
            // wake: a caller that saw the word go to zero would race for it against the thread
            // already queued, and the kernel does not leave that gap. Longest-waiting first — the
            // kernel picks by priority, there are none here, and this is the choice a replay can
            // reconstruct.
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

            // An operation this model has no answer for. Named on the timeline rather than only
            // refused, because a caller's own error message for it says nothing about which one:
            // a guest whose mutex needs an operation that is not here aborts inside its C library,
            // several frames from anything that mentions a futex.
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

    /// The content a descriptor refers to, for the disk model's crash bookkeeping.
    pub(crate) fn inode_of(&self, fd: i64) -> Option<u64> {
        self.open.get(&fd).and_then(|f| f.inode)
    }

    /// Every file on this disk, by name, with how long each is.
    ///
    /// Names rather than inode numbers, because what a reader of a report needs to be told is
    /// which file went short — and a name is also what survives a restart, where the node's whole
    /// world is rebuilt and its inode numbering starts again.
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
    /// What the kernel says about a process is not on the disk and is left out of the image, so no
    /// snapshot holds it, no crash rolls it back and no tear shortens it.
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
            directories: self.directories.clone(),
            next_inode: self.next_inode,
        }
    }

    /// Whether this file is one the kernel answers out of itself rather than one on a disk.
    pub(crate) fn is_synthetic(&self, inode: u64) -> bool {
        self.synthetic.contains(&inode)
    }

    /// Puts a file the kernel answers out of itself into this world, and answers its number.
    ///
    /// What `openat` does when a guest asks about its own process, reachable without one: a rule
    /// about which files may be on a disk needs a file that may not be.
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
    /// Cloned rather than copied by a crash that keeps a file whole, which shares its blocks.
    pub(crate) fn file_content(&self, inode: u64) -> Option<&FileBytes> {
        self.content.get(&inode)
    }

    /// Puts one file back to held content, or removes it where there was none.
    ///
    /// One file and not the disk: a failed sync loses what it was asked to make durable and says
    /// nothing about any other file, and restoring the whole disk would quietly undo writes to
    /// files the failure never touched.
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
    /// What this world answers out of itself survives, because it was never in the image: a guest
    /// that comes back from a crash and asks the system about itself is answered, exactly as one on
    /// a real machine is.
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
        self.directories = image.directories;
        // The image's own numbering, which a rewind is entitled to roll back: what was made after
        // the fork point is gone and its numbers are free again. Only a synthetic file forces the
        // counter forward, because it is still here and its number must not be handed out twice.
        self.next_inode = image.next_inode;
        for (inode, content, name) in kept {
            self.next_inode = self.next_inode.max(inode + 1);
            self.content.insert(inode, content);
            self.names.insert(name, inode);
        }
    }

    /// What the guest is told the time is: the stop's node-local reading.
    ///
    /// The single choke point for every presented timestamp, which is what lets the shared-clock
    /// injection replace all of them at once and the determinism suite prove it would be noticed.
    pub(crate) fn presented_now(ctx: &WorldCtx<'_, '_>) -> Tick {
        #[cfg(feature = "chaos-node-clock-shared")]
        return crate::chaos::shared_now(ctx);
        #[cfg(not(feature = "chaos-node-clock-shared"))]
        ctx.now()
    }

    /// What the guest is told the wall time is: the same reading until the node's clock is
    /// stepped, after which it can move back while [`Self::presented_now`] never does.
    pub(crate) fn presented_wall(ctx: &WorldCtx<'_, '_>) -> Tick {
        #[cfg(feature = "chaos-node-clock-shared")]
        return crate::chaos::shared_now(ctx);
        #[cfg(not(feature = "chaos-node-clock-shared"))]
        ctx.wall_now()
    }

    /// Whether a `clock_gettime` clock id reads wall time rather than monotonic time.
    ///
    /// `CLOCK_REALTIME`, its coarse and alarm forms, and `CLOCK_TAI` follow the wall clock and are
    /// what `settimeofday` moves; every other id counts elapsed time and never goes back.
    pub(crate) const fn reads_wall(clock_id: u64) -> bool {
        matches!(clock_id, 0 | 5 | 8 | 11)
    }

    /// The clock an absolute deadline is measured against: the wall one where the caller named it.
    ///
    /// `pthread_cond_timedwait` and `clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME)` pass wall
    /// times, and measuring one against monotonic time is off by every step the clock has taken.
    pub(crate) fn deadline_clock(ctx: &WorldCtx<'_, '_>, wall: bool) -> Tick {
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
        ctx: &mut WorldCtx<'_, '_>,
        word: u64,
        count: usize,
        bits: u32,
    ) -> Result<Vec<Wake>, KernelError> {
        let mut woken = Vec::new();
        // Reused across the whole wake rather than rebuilt per waiter released.
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
            // Which waiter goes is a decision, and every decision comes from the tape. Taking the
            // first would hide every bug that depends on wake order.
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
    fn read_word(ctx: &mut WorldCtx<'_, '_>, addr: u64) -> Result<u32, KernelError> {
        let mut buf = [0u8; 4];
        ctx.read_mem(GuestAddr(addr), &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn write_word(ctx: &mut WorldCtx<'_, '_>, addr: u64, value: u32) -> Result<(), KernelError> {
        ctx.write_mem(GuestAddr(addr), &value.to_le_bytes())
    }

    /// What the guest is told its thread is called, which is what an owned lock word holds.
    ///
    /// The same number `gettid` answers, and it has to be: a caller compares the word against its
    /// own identifier to decide whether it already holds the lock.
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
        ctx: &mut WorldCtx<'_, '_>,
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
        ctx: &mut WorldCtx<'_, '_>,
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
    fn write_timeval(ctx: &mut WorldCtx<'_, '_>, addr: GuestAddr) -> Result<[u8; 8], KernelError> {
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
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        // Waiting is not a return value, so this one call builds its own effects rather than
        // going through the shape the others share.
        if nr == sys::FUTEX {
            return self.futex_effects(ctx, args);
        }

        // A mapping and the world's copy of a file are two copies of one thing, and the calls that
        // move either of them are handled here rather than in each arm: one place to look, and one
        // place that can be wrong. Nothing happens at all for a file nobody has mapped.
        let mapped = self.mapped_inode(nr, args);
        // Where a write is about to land, taken before it lands: the non-positional forms move the
        // descriptor, so afterwards the file no longer says where they started.
        let wrote_from = mapped.and(self.open.get(&(args[0] as i64))).map_or(0, |f| f.position);
        if let Some(inode) = mapped
            && matches!(nr, sys::READ | sys::PREAD64)
            && self.is_mapped_writable(inode)
        {
            self.collect_mappings(ctx, inode)?;
        }

        // Creating a task is carried out by the host, since a real thread is what the guest needs.
        // What the model takes from the call is the word the new task will clear when it ends,
        // which is what a join waits on.
        if nr == sys::CLONE || nr == sys::CLONE3 {
            let word = if nr == sys::CLONE {
                args[3]
            } else {
                let mut buf = [0u8; 8];
                let Some(at) = offset(GuestAddr(args[0]), CLONE_ARGS_CHILD_TID) else {
                    return Ok(Effects::go(Resume::Emulate { ret: -errno::EFAULT }, SYSCALL_COST));
                };
                ctx.read_mem(at, &mut buf)?;
                u64::from_le_bytes(buf)
            };
            if word != 0 {
                self.creating.insert(ctx.task(), word);
            }
            ctx.emit(EventKind::Syscall { nr, args, ret: 0, emulated: false, data_digest: None })?;
            return Ok(Effects::go(Resume::Continue, SYSCALL_COST));
        }

        let (ret, resume, data_digest) = match nr {
            sys::CLOCK_GETTIME => {
                let presented = if Self::reads_wall(args[0]) {
                    Self::presented_wall(ctx)
                } else {
                    Self::presented_now(ctx)
                };
                let d = Self::write_timespec(ctx, GuestAddr(args[1]), presented)?;
                ctx.emit(EventKind::ClockRead { presented })?;
                (0, Resume::Emulate { ret: 0 }, Some(d))
            }
            // The oldest spelling of the question, and the coarsest: whole seconds, returned and
            // optionally written through a pointer. Read as a clock like the others, so a run that
            // asks the time this way is as recorded as one that asks it any other.
            sys::TIME => {
                let seconds = (Self::presented_wall(ctx).get() / Tick::SECOND.get()) as i64;
                let digest = if args[0] == 0 {
                    None
                } else {
                    let buf = seconds.to_le_bytes();
                    ctx.write_mem(GuestAddr(args[0]), &buf)?;
                    Some(short_digest(&buf))
                };
                let presented = Self::presented_wall(ctx);
                ctx.emit(EventKind::ClockRead { presented })?;
                (seconds, Resume::Emulate { ret: seconds }, digest)
            }
            sys::GETTIMEOFDAY => {
                let d = Self::write_timeval(ctx, GuestAddr(args[0]))?;
                let presented = Self::presented_wall(ctx);
                ctx.emit(EventKind::ClockRead { presented })?;
                (0, Resume::Emulate { ret: 0 }, Some(d))
            }
            sys::GETRANDOM => {
                // Bounded like any other transfer. The call is allowed to answer short, so a
                // request past the bound is answered in part rather than refused, which is what
                // every caller of it already copes with.
                let len = usize::try_from(args[1]).unwrap_or(0).min(MAX_TRANSFER);
                let mut bytes = vec![0u8; len];
                ctx.draw_into(&mut bytes, DrawSite::GuestRandom);
                ctx.write_mem(GuestAddr(args[0]), &bytes)?;
                (len as i64, Resume::Emulate { ret: len as i64 }, Some(short_digest(&bytes)))
            }
            sys::WRITE => {
                let len = usize::try_from(args[2]).unwrap_or(0);
                let fd = args[0] as i64;
                let (ret, d) = if self.file_behind(fd).is_none() && (fd == 1 || fd == 2) {
                    self.write_out(ctx, GuestAddr(args[1]), len)?
                } else {
                    self.write_file(ctx, fd, GuestAddr(args[1]), len, None)?
                };
                (ret, Resume::Emulate { ret }, d)
            }
            // A gathering write is a write whose bytes are named by a vector of segments. They
            // are joined here rather than answered per segment, because a short answer to one
            // segment would leave the guest to work out which bytes reached the file.
            // A gathering write with a position of its own, which is what a store uses when several
            // threads share one descriptor onto one file.
            sys::WRITEV | sys::PWRITEV if too_many_iovecs(args[2]) => {
                (-errno::EINVAL, Resume::Emulate { ret: -errno::EINVAL }, None)
            }
            sys::WRITEV | sys::PWRITEV => {
                let fd = args[0] as i64;
                let joined = Self::gather(ctx, GuestAddr(args[1]), args[2])?;
                let at = (nr == sys::PWRITEV).then_some(args[3]);
                let (ret, d) = self.write_bytes(fd, &joined, at);
                (ret, Resume::Emulate { ret }, d)
            }
            sys::PWRITE64 => {
                let len = usize::try_from(args[2]).unwrap_or(0);
                let (ret, d) =
                    self.write_file(ctx, args[0] as i64, GuestAddr(args[1]), len, Some(args[3]))?;
                (ret, Resume::Emulate { ret }, d)
            }
            sys::READ => {
                let len = usize::try_from(args[2]).unwrap_or(0);
                let (ret, d) = self.read_in(ctx, args[0] as i64, GuestAddr(args[1]), len, None)?;
                (ret, Resume::Emulate { ret }, d)
            }
            sys::PREAD64 => {
                let len = usize::try_from(args[2]).unwrap_or(0);
                let (ret, d) =
                    self.read_in(ctx, args[0] as i64, GuestAddr(args[1]), len, Some(args[3]))?;
                (ret, Resume::Emulate { ret }, d)
            }
            // `open` names its path one argument earlier than `openat`, having no descriptor for it
            // to be relative to — which nothing here is, every path being whole.
            sys::OPEN | sys::OPENAT => {
                let (dirfd, at) = if nr == sys::OPEN {
                    (AT_FDCWD, (args[0], args[1]))
                } else {
                    (dirfd_of(args[0]), (args[1], args[2]))
                };
                let ret = self.open_at(ctx, dirfd, GuestAddr(at.0), at.1)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::CLOSE => {
                self.dir_of.remove(&(args[0] as i64));
                self.rings.remove(&(args[0] as i64));
                let ret = match self.open.remove(&(args[0] as i64)) {
                    Some(file) => {
                        if let Some(inode) = file.inode {
                            self.collect(inode);
                        }
                        0
                    }
                    None => -errno::EBADF,
                };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::LSEEK => {
                let ret = self.seek(args[0] as i64, args[1] as i64, args[2]);
                (ret, Resume::Emulate { ret }, None)
            }
            sys::FSTAT => {
                let ret = self.write_stat(ctx, args[0] as i64, GuestAddr(args[1]))?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::FTRUNCATE => {
                let ret = self.truncate(args[0] as i64, args[1]);
                (ret, Resume::Emulate { ret }, None)
            }
            // Durability is a promise about what survives a crash, and nothing crashes in this
            // model: the content is already the only copy. The call is recorded because the moment
            // it happens is what a crash model built on top of this one keys off.
            sys::FSYNC | sys::FDATASYNC => {
                let ret = if self.open.contains_key(&(args[0] as i64)) { 0 } else { -errno::EBADF };
                (ret, Resume::Emulate { ret }, None)
            }
            // One task runs at a time, so an advisory lock is never contended and always granted.
            sys::FLOCK => {
                let ret = if self.open.contains_key(&(args[0] as i64)) { 0 } else { -errno::EBADF };
                (ret, Resume::Emulate { ret }, None)
            }
            // `rmdir` is the third spelling of one operation, and the only one that asks for a
            // directory by being itself rather than by carrying a flag.
            sys::UNLINK | sys::UNLINKAT | sys::RMDIR => {
                let at = if nr == sys::UNLINKAT { args[1] } else { args[0] };
                // Removing the name leaves the content, which whoever holds it open still reads
                // and writes. That is what a temporary file is.
                // `AT_REMOVEDIR` is what asks for a directory; without it, Linux refuses one.
                // Both went the same way here, so `unlink` on a directory removed it -- and a
                // store whose cleanup relies on the refusal took a path it could never take.
                const AT_REMOVEDIR: u64 = 0x200;
                let removing_dir =
                    nr == sys::RMDIR || (nr == sys::UNLINKAT && args[2] & AT_REMOVEDIR != 0);
                let dirfd = if nr == sys::UNLINKAT { dirfd_of(args[0]) } else { AT_FDCWD };
                let ret = match read_c_string(ctx, GuestAddr(at))? {
                    PathArg::Named(name) => match self.under(dirfd, &name) {
                        Err(errno) => errno,
                        Ok(path) => match self.names.get(&path).copied() {
                            Some(inode) if self.directories.contains(&inode) && !removing_dir => {
                                -errno::EISDIR
                            }
                            Some(inode) if !self.directories.contains(&inode) && removing_dir => {
                                -errno::ENOTDIR
                            }
                            Some(inode) => {
                                self.names.remove(&path);
                                // The set of directories is a second index onto the same names, so
                                // a directory taken out of one had
                                // to leave the other: left behind, its
                                // number went on being a directory to every later check.
                                self.directories.remove(&inode);
                                self.collect(inode);
                                0
                            }
                            None => -errno::ENOENT,
                        },
                    },
                    other => other.errno(),
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // Three spellings: one that takes no directories and no flags, one that takes
            // directories, and one that takes both.
            sys::RENAME | sys::RENAMEAT | sys::RENAMEAT2 => {
                let (from, to) =
                    if nr == sys::RENAME { (args[0], args[1]) } else { (args[1], args[3]) };
                // `renameat2` carries flags, and they were not read: a caller asking for no
                // replacement silently clobbered.
                let flags = if nr == sys::RENAMEAT2 { args[4] } else { 0 };
                let dirs = if nr == sys::RENAME {
                    (AT_FDCWD, AT_FDCWD)
                } else {
                    (dirfd_of(args[0]), dirfd_of(args[2]))
                };
                let ret = self.rename(ctx, dirs, GuestAddr(from), GuestAddr(to), flags)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Advice about future access changes nothing the guest can observe here.
            sys::FADVISE64 => (0, Resume::Emulate { ret: 0 }, None),
            // Nothing here is a terminal, which is what a caller of this is asking.
            sys::IOCTL => (-errno::ENOTTY, Resume::Emulate { ret: -errno::ENOTTY }, None),
            // Only what this model stores exists, and it is reached by opening it rather than by
            // asking about it.
            // Answered from the same names `stat` and `openat` are: a guest asking "is my database
            // there" and being told no, while `stat` on the same path said yes, concluded it was a
            // fresh install and initialised over the state it had been given.
            sys::ACCESS | sys::FACCESSAT2 => {
                let (dirfd, at) = if nr == sys::ACCESS {
                    (AT_FDCWD, args[0])
                } else {
                    (dirfd_of(args[0]), args[1])
                };
                let ret = match read_c_string(ctx, GuestAddr(at))? {
                    PathArg::Named(name) => match self.under(dirfd, &name) {
                        Ok(path) if self.names.contains_key(&self.resolve(&path)) => 0,
                        Ok(_) => -errno::ENOENT,
                        Err(errno) => errno,
                    },
                    other => other.errno(),
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // Nothing in this model is a symbolic link, so nothing has a target to read.
            sys::READLINK => {
                let ret =
                    self.read_link(ctx, AT_FDCWD, GuestAddr(args[0]), GuestAddr(args[1]), args[2])?;
                (ret, Resume::Emulate { ret }, None)
            }
            // `symlinkat` names its link in the third argument, where `symlink` names it in the
            // second. The directory descriptor between them is not read: this model has one
            // directory tree and every name in it is resolved from the working directory.
            // A duplicate onto a number that is already open closes what was there, which is the
            // whole point of the call: the number is the thing being replaced.
            sys::DUP | sys::DUP2 | sys::DUP3 => {
                let onto = (nr != sys::DUP).then(|| args[1] as i64);
                let ret = self.duplicate(args[0] as i64, onto, nr == sys::DUP3);
                (ret, Resume::Emulate { ret }, None)
            }
            sys::SYMLINK | sys::SYMLINKAT => {
                let (dirfd, at) = if nr == sys::SYMLINK {
                    (AT_FDCWD, args[1])
                } else {
                    (dirfd_of(args[1]), args[2])
                };
                let ret = self.make_link(ctx, dirfd, GuestAddr(args[0]), GuestAddr(at))?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Descriptor flags are accepted and reported as clear: nothing this model offers
            // behaves differently for any of them.
            // A duplicate is a second descriptor onto the same open file, and it has to be a
            // descriptor: answering the flag commands' zero to a duplication request hands the
            // guest standard input as its copy, and every later use of it goes somewhere else
            // entirely. Everything else about a descriptor this model holds is already true —
            // nothing here is inherited across an exec, and nothing here blocks.
            sys::FCNTL => {
                let ret = match self.open.get(&(args[0] as i64)).copied() {
                    None => -errno::EBADF,
                    Some(file) => match args[1] as u32 {
                        fcntl_cmd::DUPFD | fcntl_cmd::DUPFD_CLOEXEC => self.attach(file),
                        _ => 0,
                    },
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // Answered from the field a scenario sets. It answered a hard-coded `/`
            // while the field was written by `with_cwd` and read by nobody, so
            // `state.cwd` was a key the configuration parsed, validated and did
            // nothing with -- a configuration that lies, by this repository's own
            // definition.
            sys::GETCWD => {
                let mut answer = self.cwd.clone().into_bytes();
                answer.push(0);
                let ret = Self::write_bytes_out(ctx, GuestAddr(args[0]), args[1], &answer)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::UNAME => {
                let ret = Self::write_uname(ctx, GuestAddr(args[0]))?;
                (ret, Resume::Emulate { ret }, None)
            }
            // One identity, and it is the same on every machine. Real credentials belong to
            // whoever happens to run the tool.
            sys::GETUID
            | sys::GETGID
            | sys::GETEUID
            | sys::GETEGID
            | sys::GETPPID
            | sys::GETPGRP
            | sys::SCHED_GETPARAM
            | sys::SCHED_GETSCHEDULER => (0, Resume::Emulate { ret: 0 }, None),
            // A fatal signal a task sends itself is the one case where delivering nothing would be
            // wrong: `abort` retries until it dies, so refusing to let it die is a loop with no
            // end.
            // **`tgkill` names a thread, and this answered about the caller whatever it named.**
            // A runtime that kills one of its own threads took the whole task down instead, so a
            // guest doing something ordinary was recorded as having died of it. Only a signal aimed
            // at the caller is answered here; one aimed elsewhere is refused rather than
            // misdirected and delivering to another task is not something this world
            // can do.
            // A signal to another thread waits for that thread's next stop, where the kernel
            // delivers it under the thread's own disposition. A thread parked in a wait this world
            // holds is released from it with `EINTR`, as a signal releases it on Linux.
            sys::TGKILL if args[1] as u32 != Self::thread_id(ctx.task()) => {
                let target = TaskId(i64::from(args[1] as u32).wrapping_sub(GUEST_PID) as u32);
                let alive = self.born.contains(&target) && !self.ended.contains(&target);
                let signal = args[2] as u32;
                let ret = if !alive {
                    -errno::ESRCH
                } else if signal == 0 || signal > 64 {
                    if signal == 0 { 0 } else { -errno::EINVAL }
                } else {
                    self.pending_signals.entry(target).or_default().push(signal);
                    let interrupted = self.interrupt_wait(target);
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: true,
                        data_digest: None,
                    })?;
                    return Ok(Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST)
                        .releasing(interrupted));
                };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::TGKILL | sys::KILL => {
                let signal = if nr == sys::KILL { args[1] } else { args[2] } as u32;
                if FATAL_SIGNALS.contains(&signal) {
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: true,
                        data_digest: None,
                    })?;
                    return Ok(Effects::go(
                        Resume::Terminate { signal: Some(signal) },
                        SYSCALL_COST,
                    ));
                }
                (0, Resume::Emulate { ret: 0 }, None)
            }
            // No memory figures, so nothing to report.
            // Zero-filled before the answer, because a caller reads the structure this writes
            // into: answering success and leaving it alone handed the guest whatever its own stack
            // held, which is host state reaching a run through a call that claimed to have set it.
            sys::SYSINFO => {
                ctx.write_mem(GuestAddr(args[0]), &[0u8; SYSINFO_BYTES])?;
                (0, Resume::Emulate { ret: 0 }, None)
            }
            // What a task has spent, which here is nothing. This model does not account processor
            // time to a task at all — a run's cost is its virtual clock, and the clock is the
            // world's rather than any task's — so every field is zero. Zero is a measurement this
            // model can stand behind; a figure made up for it is one somebody would eventually
            // divide by.
            sys::GETRUSAGE => {
                ctx.write_mem(GuestAddr(args[1]), &[0u8; RUSAGE_BYTES])?;
                (0, Resume::Emulate { ret: 0 }, None)
            }
            // What this machine allows, which is the same machine everywhere. `prlimit64` names
            // its resource one argument along and may also carry a new limit: the new one is
            // accepted and forgotten, because a guest raising its own descriptor limit is asking
            // for room it already has here, and a guest lowering it is asking this model to refuse
            // things it has no reason to refuse.
            sys::GETRLIMIT | sys::SETRLIMIT | sys::PRLIMIT64 => {
                let (resource, out) = match nr {
                    sys::GETRLIMIT => (args[0], args[1]),
                    sys::SETRLIMIT => (args[0], 0),
                    _ => (args[1], args[3]),
                };
                if out != 0 {
                    let (soft, hard) = limit_for(resource);
                    let mut answer = [0u8; RLIMIT_BYTES];
                    answer[..8].copy_from_slice(&soft.to_le_bytes());
                    answer[8..].copy_from_slice(&hard.to_le_bytes());
                    ctx.write_mem(GuestAddr(out), &answer)?;
                }
                (0, Resume::Emulate { ret: 0 }, None)
            }
            sys::GETDENTS64 => {
                let ret =
                    self.list_directory(ctx, args[0] as i64, GuestAddr(args[1]), args[2] as usize)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // `mkdirat` names its path one argument along, after the descriptor it would have been
            // relative to — which nothing here is, every path being whole.
            sys::MKDIR | sys::MKDIRAT => {
                let (dirfd, at) = if nr == sys::MKDIR {
                    (AT_FDCWD, args[0])
                } else {
                    (dirfd_of(args[0]), args[1])
                };
                let ret = self.make_directory(ctx, dirfd, GuestAddr(at))?;
                (ret, Resume::Emulate { ret }, None)
            }
            // A pipe is a stream pair, and streams are the cluster's: a world with a `[cluster]`
            // answers this call, and one without has no network to make the pair from.
            sys::PIPE2 => {
                let ret = self.unimplemented(ctx, nr)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Readiness as Linux reports it for what this world holds: a file, a directory and the
            // standard descriptors are always ready for what was asked, a descriptor nobody holds
            // is `POLLNVAL`. Where nothing is ready the call is the wait it asked for: its timeout
            // spent in virtual time, or, with none, a wait nothing here can end — judged as any
            // other wait, which is a deadlock once every task is in one.
            sys::POLL => {
                let ready = self.poll_ready(ctx, GuestAddr(args[0]), args[1])?;
                let timeout = args[2] as i32;
                if ready != 0 || timeout == 0 {
                    (ready, Resume::Emulate { ret: ready }, None)
                } else {
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: true,
                        data_digest: None,
                    })?;
                    if timeout < 0 {
                        return Ok(Effects::wait(SYSCALL_COST));
                    }
                    let ahead = Tick(u64::from(timeout.unsigned_abs()).saturating_mul(1_000_000));
                    return Ok(Effects::go(Resume::Emulate { ret: 0 }, ahead));
                }
            }
            // Sleeping costs virtual time and no real time at all, which is what makes a test that
            // waits a day finish in the time it takes to run the code around the waiting.
            sys::NANOSLEEP | sys::CLOCK_NANOSLEEP => {
                let request = if nr == sys::NANOSLEEP { args[0] } else { args[2] };
                let absolute = nr == sys::CLOCK_NANOSLEEP && args[1] & TIMER_ABSTIME != 0;
                let until = Self::read_timespec(ctx, GuestAddr(request))?;
                let ahead = if absolute {
                    until.saturating_sub(Self::deadline_clock(ctx, Self::reads_wall(args[0])).get())
                } else {
                    until
                };
                ctx.emit(EventKind::ClockRead { presented: Self::presented_now(ctx) })?;
                return Ok(Effects::go(Resume::Emulate { ret: 0 }, Tick(ahead)));
            }
            // A guest that asks how it should schedule itself is told one processor, because that
            // is what it has: the scheduler admits one task at a time.
            sys::SCHED_GETAFFINITY => {
                let ret = Self::write_affinity(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // A yield is the guest asking to be interrupted, which is the one switch point it can
            // name itself.
            sys::SCHED_YIELD => {
                ctx.emit(EventKind::Syscall {
                    nr,
                    args,
                    ret: 0,
                    emulated: true,
                    data_digest: None,
                })?;
                return Ok(Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST).yielding());
            }
            // A ring under `[linux] io_uring = "ring"`; otherwise a kernel without one, answered
            // `ENOSYS` so the guest takes the fallback it keeps for that kernel.
            sys::IO_URING_SETUP if self.io_uring => {
                let ret = self.open_ring(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::IO_URING_SETUP => {
                let ret = self.decline_ring(ctx)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Registering with a ring is an optimisation and nothing else: files and buffers
            // handed over once so later submissions can name them by index. A caller told no takes
            // the ordinary path, which is the path this world models, and every caller keeps that
            // path because registration is refused by kernels that limit locked memory.
            sys::IO_URING_REGISTER if self.io_uring => {
                let ret = -errno::EINVAL;
                (ret, Resume::Emulate { ret }, None)
            }
            // A ring this world opened is entered through the cluster, which carries its entries
            // by the route each takes; reaching here, the descriptor is not a ring.
            sys::IO_URING_ENTER if self.io_uring => {
                (-errno::EBADF, Resume::Emulate { ret: -errno::EBADF }, None)
            }
            // With no ring to have opened, reaching either call is a state this model cannot
            // account for, and it ends the run under the ring's own code.
            sys::IO_URING_ENTER | sys::IO_URING_REGISTER => {
                let ret = self.unmodelled(ctx, nr, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Scheduling niceness, which this world does not have: one task runs at a time and the
            // order is the tape's. Answered as the middling value the call reports for a process
            // nobody has reniced -- the raw call returns twenty less the niceness, so twenty is
            // none of it, and a guest reading its own priority sees what it would have on a
            // machine where nothing had touched it.
            sys::GETPRIORITY => (20, Resume::Emulate { ret: 20 }, None),
            // And setting it is accepted and has no effect, which is the truth rather than a
            // convenience: what a task is worth to the scheduler is drawn from the tape here, and
            // a niceness the world honoured would be a second scheduler quietly disagreeing with
            // the first. A caller that reads its priority back gets the same answer as before,
            // because that is what this world's scheduling is.
            sys::SETPRIORITY => (0, Resume::Emulate { ret: 0 }, None),
            // Scheduling policy, answered as an unprivileged process is: the ordinary, batch and
            // idle policies are accepted and change nothing, for the reason niceness changes
            // nothing; a real-time policy is refused with `EPERM`, which is what a task without the
            // capability or a real-time limit gets, and every caller that asks for one has a path
            // for that refusal because most machines give it.
            sys::SCHED_SETSCHEDULER => {
                /// `SCHED_RESET_ON_FORK`, a flag ORed into the policy.
                const RESET_ON_FORK: u64 = 0x4000_0000;
                let mut priority = [0u8; 4];
                ctx.read_mem(GuestAddr(args[2]), &mut priority)?;
                let ret = match args[1] & !RESET_ON_FORK {
                    // SCHED_OTHER, SCHED_BATCH, SCHED_IDLE, whose one priority is zero.
                    0 | 3 | 5 if i32::from_le_bytes(priority) == 0 => 0,
                    0 | 3 | 5 => -errno::EINVAL,
                    // SCHED_FIFO, SCHED_RR, SCHED_DEADLINE.
                    1 | 2 | 6 => -errno::EPERM,
                    _ => -errno::EINVAL,
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // The ordinary policy has one priority, zero; asking for another is a mistake the
            // kernel answers `EINVAL`.
            sys::SCHED_SETPARAM => {
                let mut priority = [0u8; 4];
                ctx.read_mem(GuestAddr(args[1]), &mut priority)?;
                let ret = if i32::from_le_bytes(priority) == 0 { 0 } else { -errno::EINVAL };
                (ret, Resume::Emulate { ret }, None)
            }
            // A hint about what will be wanted soon. Nothing here reads ahead, and a hint acted on
            // by nobody is answered exactly as a kernel that declined to act on it would.
            sys::READAHEAD => {
                let ret = if self.is_file(args[0] as i64) { 0 } else { -errno::EBADF };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::STATX => {
                let ret = self.statx_at(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::PRCTL => {
                let ret = self.prctl(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Watching files for change has a fallback in every program that uses it, polling,
            // and a world whose files change only by the guest's own calls has nothing a watch
            // would report that the guest does not already know.
            nr if sys::INOTIFY.contains(&nr) => {
                let ret = self.unimplemented(ctx, nr)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::CHMOD | sys::FCHMODAT => {
                let (dirfd, path, mode) = if nr == sys::CHMOD {
                    (AT_FDCWD as u64, args[0], args[1])
                } else {
                    (args[0], args[1], args[2])
                };
                let ret = match self.stat_target(ctx, dirfd, path, 0)? {
                    Ok((Some(inode), _)) => {
                        self.modes.insert(inode, (mode as u32) & 0o7777);
                        0
                    }
                    Ok((None, _)) => 0,
                    Err(errno) => errno,
                };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::FCHMOD => {
                let ret = match self.open.get(&(args[0] as i64)) {
                    Some(file) => {
                        if let Some(inode) = file.inode {
                            self.modes.insert(inode, (args[1] as u32) & 0o7777);
                        }
                        0
                    }
                    None => -errno::EBADF,
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // Space reserved, or a file extended with zeros; nothing else this world can promise.
            // Punching holes and collapsing ranges are refused as a filesystem without them
            // refuses them, and every caller keeps the path for that.
            sys::FALLOCATE => {
                /// `FALLOC_FL_KEEP_SIZE`.
                const KEEP_SIZE: u64 = 1;
                let fd = args[0] as i64;
                let ret = if !self.open.contains_key(&fd) {
                    -errno::EBADF
                } else if !self.is_file(fd) {
                    -errno::ENODEV
                } else {
                    match args[1] {
                        0 => {
                            let end = args[2].saturating_add(args[3]);
                            let length = self.length_of(fd).unwrap_or(0);
                            if end > length { self.truncate(fd, end) } else { 0 }
                        }
                        KEEP_SIZE => 0,
                        _ => -errno::EOPNOTSUPP,
                    }
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // Write-back started and nothing promised: the call says so itself, so it marks
            // nothing durable here.
            sys::SYNC_FILE_RANGE => {
                let fd = args[0] as i64;
                let ret = if !self.open.contains_key(&fd) {
                    -errno::EBADF
                } else if self.is_file(fd) {
                    0
                } else {
                    -errno::ESPIPE
                };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::NEWFSTATAT => {
                let ret = self.stat_at(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // A descriptor nobody holds is `EBADF`, not a missing name: `ENOENT` sent a caller
            // looking for a path it had not given.
            sys::FSTATFS if !self.open.contains_key(&(args[0] as i64)) => {
                (-errno::EBADF, Resume::Emulate { ret: -errno::EBADF }, None)
            }
            sys::STATFS | sys::FSTATFS => {
                let named = if nr == sys::FSTATFS {
                    self.open.contains_key(&(args[0] as i64))
                } else {
                    match read_c_string(ctx, GuestAddr(args[0]))? {
                        PathArg::Named(name) => self
                            .under(AT_FDCWD, &name)
                            .is_ok_and(|path| self.names.contains_key(&self.resolve(&path))),
                        _ => false,
                    }
                };
                let ret = self.write_statfs(ctx, named, GuestAddr(args[1]))?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::READLINKAT => {
                let ret = self.read_link(
                    ctx,
                    dirfd_of(args[0]),
                    GuestAddr(args[1]),
                    GuestAddr(args[2]),
                    args[3],
                )?;
                (ret, Resume::Emulate { ret }, None)
            }
            // One node, one process, one identity. A host pid would differ on every run and is
            // exactly the kind of value a guest folds into a filename or a seed.
            sys::GETPID => (GUEST_PID, Resume::Emulate { ret: GUEST_PID }, None),
            // Task identifiers are the run's own, and they are what a thread stores in memory and
            // waits on, so a host number here would make the run differ from itself.
            sys::GETTID => {
                let ret = i64::from(ctx.task().get()) + GUEST_PID;
                (ret, Resume::Emulate { ret }, None)
            }
            // Ending is the one thing the guest does for itself. Emulating the call would leave
            // the task alive with a return value it never asked for; letting it through ends the
            // task, and the exit arrives as its own stop.
            sys::EXIT | sys::EXIT_GROUP => (0, Resume::Continue, None),
            // Reaches here only where it named a file: the filter admits an anonymous mapping,
            // which touches nothing this world holds.
            sys::MMAP => match self.map_file(ctx, args) {
                Some(resume) => {
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: false,
                        data_digest: None,
                    })?;
                    return Ok(Effects::go(resume, SYSCALL_COST));
                }
                // A descriptor this world does not own has no file behind it to show.
                None => {
                    let ret = self.unmodelled(ctx, nr, args)?;
                    (ret, Resume::Emulate { ret }, None)
                }
            },
            // Let through to the host, which owns the address space; what this model takes from it
            // is that a window it was keeping truthful is about to stop existing.
            sys::MUNMAP => {
                self.unmap(ctx, args[0], args[1])?;
                (0, Resume::Continue, None)
            }
            // A mapping of no file, or of a file nobody is writing through: nothing to carry back,
            // and nothing to make durable that a write did not already make pending. A cluster
            // intercepts this call before it arrives whenever there is.
            sys::MSYNC => (0, Resume::Emulate { ret: 0 }, None),
            other => {
                let ret = self.unmodelled(ctx, other, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
        };

        if let Some(inode) = mapped {
            let changed = match nr {
                sys::WRITE | sys::WRITEV if ret > 0 => wrote_from..wrote_from + ret as u64,
                sys::PWRITE64 | sys::PWRITEV if ret > 0 => args[3]..args[3] + ret as u64,
                // A file that changed length changed which of its bytes exist at all, and the
                // window over it has to be told about the whole of what it now shows.
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
    /// Nothing at all where nobody has: the whole of this machinery costs a lookup per call on a
    /// world where no mapping exists, which is every world until one does.
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
    /// First-touch order rather than the address, because an address is where a lock lives on this
    /// machine and the trace must not depend on that ([`bohr_types::SyncWordId`]). The order is a
    /// function of what the run did, so two runs that behaved alike number alike.
    fn sync_word(&mut self, word: GuestAddr) -> SyncWordId {
        let next = self.sync_words.len() as u32;
        *self.sync_words.entry(word).or_insert(SyncWordId(next))
    }

    /// Turns a `futex` call into effects, recording the waiting and the waking as events.
    fn futex_effects(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
/// Read in large steps and narrowed on failure: a long read past the end of a mapping is refused as
/// a whole, and almost every path fits in the first step, so the common case costs one crossing of
/// the boundary rather than one per eight bytes.
impl MinimalWorld {
    /// The absolute name a path the guest gave stands for.
    ///
    /// **Names here are flat strings, so a relative one has to be made absolute before it is used
    /// as a key.** Taken literally, `db/state` and `/db/state` were two unrelated files, and a
    /// guest that opened one and asked about the other was answered about neither. Resolved
    /// against the working directory a scenario named, which is what `getcwd` now reports too.
    fn absolute(&self, path: &str) -> String {
        if path.starts_with('/') {
            return path.to_owned();
        }
        format!("{}/{path}", self.cwd.trim_end_matches('/'))
    }

    /// Where a relative name is taken from.
    ///
    /// An absolute name and `AT_FDCWD` both mean the working directory decides; any other number
    /// means the call named a directory it had already opened, and that is the case this model
    /// answered by ignoring the number entirely. Every relative name went to the working
    /// directory, so a guest that opens a directory once and writes everything under it by
    /// descriptor -- which is RocksDB, and `std::fs` wherever it can -- had its files put
    /// somewhere else and was told the call succeeded. A refusal would have been better: this
    /// parted what the guest believed it had written from what the world held, without a single
    /// call reporting anything wrong.
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
            // A descriptor that is open on something that is not a directory, and one that is not
            // open at all, are different mistakes and Linux names them differently.
            None if self.open.contains_key(&dirfd) => Err(-errno::ENOTDIR),
            None => Err(-errno::EBADF),
        }
    }

    /// The name a path ends at, following symbolic links at the last component.
    ///
    /// Only at the last component, and only from the calls that follow one. `unlink`, `rename` and
    /// `readlink` act on the link itself, and a resolver applied to all of them would delete the
    /// target instead of the name -- a store rotating a log by unlinking the stable name would
    /// lose the file the name pointed at.
    ///
    /// A relative target resolves against the directory the link sits in, which is the whole point
    /// of one: `validator.log -> validator-9.log` names a file beside itself, and resolving it
    /// against the working directory would send it somewhere the guest never wrote.
    fn resolve(&self, path: &str) -> String {
        // Linux answers `ELOOP` past forty; this stops sooner and lands on a name that is still a
        // link, which every caller here already answers `ENOENT` for. A guest that builds a cycle
        // gets a refusal either way, and the number is this model's rather than the kernel's.
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
/// Three answers rather than two, because the guest has three things it can hand over and Linux
/// answers each of them differently. The one that was missing is the null pointer: `glibc` probes
/// for `statx` by calling it with a null path and reads `EFAULT` as "the kernel has this call",
/// then falls back to `newfstatat` with a null path for the same purpose. Both were refused here
/// as unreadable memory, which ends the run and blames the tool -- so no program linked against
/// `glibc` got past its own startup, measured on `solana-test-validator` and on `python3`.
///
/// What this does not do: answer for a pointer that is not null and still cannot be read. That
/// stays a refusal. Linux answers `EFAULT` there too, but from inside this model the two cases
/// are not the same evidence: address zero is a guest handing over nothing on purpose, and any
/// other address that cannot be read is as likely to be a supervisor that has lost the guest's
/// memory, which is a defect worth ending a run over.
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
    ctx: &mut WorldCtx<'_, '_>,
    addr: GuestAddr,
) -> Result<PathArg, KernelError> {
    // Down to one byte at the end. The narrowing exists for a path near the top of the stack --
    // the strings a program is started with -- and it stopped at eight, so a name shorter than
    // eight bytes sitting within seven of the end of its mapping was read by no step at all and
    // the run was refused for a path it could perfectly well have read.
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
    /// A timer fires only into the wait that armed it: the task must still be in that wait, by its
    /// number, and on the word it named. Checked when it fires, which is what makes cancelling it
    /// on a wake unnecessary.
    fn on_deadline(&mut self, ctx: &mut WorldCtx<'_, '_>) -> Result<DueEffects, KernelError> {
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
            WorldModelId::new("minimal-survey", 2)
        } else {
            WorldModelId::new("minimal", 2)
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

        // A task can end while queued on a word, by its group exiting around it. Left in the queue
        // it would be chosen by a later wake, and released into a run it is no longer part of.
        self.waiters.retain(|_, queue| {
            queue.retain(|w| w.task != task);
            !queue.is_empty()
        });

        let Some(word) = self.exit_word.remove(&task) else {
            return Vec::new();
        };
        // A thread ending releases everyone waiting on its word, and all of them at once: the
        // kernel clears the word before waking, so no waiter can find the condition still true.
        // There is nothing to choose between them, so nothing is drawn.
        self.waiters
            .remove(&word)
            .unwrap_or_default()
            .into_iter()
            .map(|w| Wake { task: w.task, ret: 0 })
            .collect()
    }

    fn handle(&mut self, stop: &Stop, ctx: &mut WorldCtx<'_, '_>) -> Result<Effects, KernelError> {
        // A node's first task is nobody's clone, so it is known from its first stop here: without
        // it a thread signalling its own main thread was told the thread did not exist, on every
        // node but the first and after every restart.
        self.born.insert(ctx.task());
        match &stop.kind {
            // `exit_group` ends the process, so every task of this node goes with it — including
            // the ones parked in a call this model is holding. Said here rather than left to the
            // task's own exit: a parked task makes no stop, so nothing else would ever notice it
            // had gone, and the run would be judged as one where every task is waiting.
            StopKind::SyscallEntry { nr, args } if *nr == sys::EXIT_GROUP => {
                let effects = self.syscall(*nr, *args, ctx)?;
                let ended = NodeOp::Ended { node: ctx.node(), by: ctx.task() };
                Ok(Effects { node_ops: vec![ended], ..effects })
            }
            StopKind::SyscallEntry { nr, args } => {
                let effects = self.syscall(*nr, *args, ctx)?;
                Ok(self.with_pending_signal(ctx.task(), effects))
            }

            // The only call this model replaces rather than answers is a mapping, and the address
            // it landed at exists only here.
            StopKind::SyscallExit { ret } => self.mapping_placed(ctx, *ret),

            // The processor is this world's, not the host's: one invented machine answered from a
            // committed table, so a guest asking what it runs on hears the same thing on every
            // host. Without it the guest reads the real silicon and branches on it -- which is
            // how a bundle produced on one machine stops replaying on another.
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

            // The counter reads the same virtual clock as any other time source, so a guest cannot
            // tell them apart and cannot use one to detect the other being simulated.
            StopKind::TimestampCounter { .. } => {
                let now = Self::presented_now(ctx);
                let mut regs = ctx.regs()?;
                regs.rax = now.get() & 0xFFFF_FFFF;
                regs.rdx = now.get() >> 32;
                ctx.set_regs(&regs)?;
                ctx.emit(EventKind::ClockRead { presented: now })?;
                Ok(Effects::go(Resume::Continue, SYSCALL_COST))
            }

            // A signal this world sent comes back from the kernel on its way to the task, and is
            // let through: the task's own disposition decides what it does.
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

            // A signal that would end the task ends it. Letting the task carry on leaves it to
            // fault on the same instruction forever, and handing the signal back for delivery does
            // not end it either: the fault is re-raised the moment the instruction runs again.
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

    fn fault_catalog(&self) -> &FaultCatalog {
        &self.catalog
    }
}
