//! The backend boundary: guest execution in terms of guest operations.
//!
//! A task stopped at a syscall, a task resumed with a substituted return value, memory read at a
//! guest address. No host mechanism appears here, and no layer above branches on the
//! implementation, so the mechanism can be replaced without touching them.
//! See `docs/ARCHITECTURE.md` §5.2 and `docs/API.md` §3.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use bohr_types::{CoverageMap, DEFAULT_COVERAGE_MAP_BYTES};
use bohr_types::{GuestAddr, NodeId, TaskId};

/// A source of guest execution that can be stopped, inspected and resumed deterministically.
pub trait DeterminismBackend {
    /// What this implementation can and cannot do.
    fn capabilities(&self) -> BackendCapabilities;

    /// Creates the initial task of a node.
    ///
    /// Returns once the guest is stopped before its first instruction, so the world can be
    /// installed before anything runs.
    fn spawn(&mut self, node: NodeId, spec: &GuestSpec) -> Result<TaskId, BackendError>;

    /// Runs until exactly one task stops, or reports that none remain.
    ///
    /// At most one task executes at a time. `None` means every task has ended, a normal end.
    fn next_stop(&mut self) -> Result<Option<Stop>, BackendError>;

    /// Lets a stopped task continue.
    fn resume(&mut self, task: TaskId, action: Resume) -> Result<(), BackendError>;

    /// Copies out of guest memory.
    fn read_mem(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        out: &mut [u8],
    ) -> Result<(), BackendError>;

    /// Copies several pieces of guest memory into `out`, one after another; `out` is as long as
    /// the pieces together.
    ///
    /// One crossing where the backend can make one; by default, one read per piece.
    fn read_mem_vectored(
        &mut self,
        task: TaskId,
        pieces: &[(GuestAddr, usize)],
        out: &mut [u8],
    ) -> Result<(), BackendError> {
        let mut at = 0;
        for (addr, len) in pieces {
            self.read_mem(task, *addr, &mut out[at..at + len])?;
            at += len;
        }
        Ok(())
    }

    /// Copies into guest memory.
    fn write_mem(&mut self, task: TaskId, addr: GuestAddr, data: &[u8])
    -> Result<(), BackendError>;

    /// Reads the register file of a stopped task.
    fn regs(&mut self, task: TaskId) -> Result<Regs, BackendError>;

    /// Replaces the register file of a stopped task.
    fn set_regs(&mut self, task: TaskId, regs: &Regs) -> Result<(), BackendError>;

    /// Mapped regions, as the guest sees them.
    fn mappings(&mut self, task: TaskId) -> Result<Vec<Mapping>, BackendError>;

    /// Ends a task without letting it run again.
    ///
    /// Nothing more is reported about the task afterwards, including its exit.
    fn kill(&mut self, task: TaskId) -> Result<(), BackendError>;

    /// What the guest executed, where this implementation can observe it.
    ///
    /// `None` means this build cannot observe execution. An empty map means the guest ran without
    /// the coverage runtime; see [`CoverageMap::is_empty`].
    ///
    /// Read after a run, so a search never depends on when it sampled. See `docs/API.md` §5.5.
    /// Takes `&mut self` because it copies the counters out of guest-written memory.
    fn coverage(&mut self) -> Option<&CoverageMap> {
        None
    }

    /// How many edges the guest was instrumented with, where the guest says.
    ///
    /// Shows whether the map is big enough: with fewer counters than edges, two edges share a
    /// counter and their paths merge.
    ///
    /// Zero for a guest without the coverage runtime.
    fn coverage_edges(&self) -> u64 {
        0
    }

    /// Releases every stopped guest to an external debugger, and says how to reach each one.
    ///
    /// Guarded by [`BackendCapabilities::hands_over_to_debugger`]. The default returns an empty
    /// list: a synthetic guest has no process to attach to.
    ///
    /// This ends the run. Nothing may be resumed, read, written or stopped afterwards, and dropping
    /// the backend ends the guest.
    ///
    /// # Errors
    ///
    /// What stopped the hand-over. Guests released before the failure stay released.
    fn hand_over(&mut self) -> Result<Vec<DebuggerTarget>, BackendError> {
        Ok(Vec::new())
    }
}

/// Where an external debugger is told to look, and what it will not be able to do there.
///
/// Opaque: it carries a command a person can run and the limits of the session, never a host
/// process identifier.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DebuggerTarget {
    node: NodeId,
    command: String,
    limits: Vec<String>,
}

impl DebuggerTarget {
    /// A target a debugger can be pointed at.
    ///
    /// The command must run exactly as written.
    #[must_use]
    pub fn new(node: NodeId, command: String, limits: Vec<String>) -> Self {
        Self { node, command, limits }
    }

    /// Which node this process is.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// The command that attaches a debugger to it.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// What the session cannot do, one sentence each.
    #[must_use]
    pub fn limits(&self) -> &[String] {
        &self.limits
    }
}

/// The stack limit every guest runs under and is told of, as Linux's default.
///
/// The host's own limit would govern the guest while the world answered another, and an unlimited
/// stack also switches the kernel to the legacy mapping layout.
pub const GUEST_STACK_BYTES: u64 = 8 << 20;

/// What to execute, and in what environment.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct GuestSpec {
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Environment, in the order it is presented to the guest.
    ///
    /// Built from configuration, so host values such as `PWD` and `HOSTNAME` stay out.
    pub env: Vec<(String, String)>,
    /// The file executed, where it is not `argv[0]`.
    ///
    /// Used for a program the tool supplies, so the guest sees a fixed `argv[0]` and not the host
    /// path.
    pub executable: Option<String>,
}

/// A task has stopped, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Stop {
    /// The task that stopped.
    pub task: TaskId,
    /// What it stopped on.
    pub kind: StopKind,
}

/// The reason a task stopped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StopKind {
    /// About to execute a syscall, which has not happened yet.
    SyscallEntry {
        /// Syscall number.
        nr: u32,
        /// Argument registers, in ABI order.
        args: [u64; 6],
    },
    /// A syscall that was allowed through to the host has returned.
    SyscallExit {
        /// What the host returned.
        ret: i64,
    },
    /// The timestamp counter was read.
    TimestampCounter {
        /// Whether the `rdtscp` form was used.
        rdtscp: bool,
    },
    /// The processor identification instruction was executed.
    CpuIdentification {
        /// Primary leaf.
        leaf: u32,
        /// Sub-leaf.
        subleaf: u32,
    },
    /// A new task appeared and is stopped before its first instruction.
    TaskCreated {
        /// The task that created it.
        parent: TaskId,
        /// Whether it shares an address space with its parent.
        shares_memory: bool,
        /// Whether it is a thread of its parent's process, ending when that process ends.
        thread: bool,
    },
    /// A task ended.
    TaskExited {
        /// How it ended.
        status: GuestExit,
    },
    /// A signal the guest would receive.
    Signal {
        /// Signal number.
        signal: u32,
    },
    /// The progress budget from [`Resume::ContinueWithBudget`] ran out.
    BudgetExhausted {
        /// Progress units actually consumed.
        consumed: u64,
    },
    /// A fault the guest did not handle.
    MemoryFault {
        /// Address that faulted.
        addr: GuestAddr,
    },
}

/// How a guest task ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuestExit {
    /// Returned a status code.
    Exited(i32),
    /// Terminated by a signal.
    Signalled(u32),
}

/// What a stopped task should do next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Resume {
    /// Run until the next stop.
    Continue,
    /// Run until the next stop or until `budget` progress units elapse.
    ContinueWithBudget {
        /// Progress units, in the unit reported by [`BackendCapabilities::progress_unit`].
        budget: u64,
    },
    /// Do not execute the syscall; return `ret` to the guest.
    ///
    /// The normal path: the guest talks to the world, and a syscall reaches the host kernel only
    /// by a per-syscall exception.
    Emulate {
        /// What the guest receives.
        ret: i64,
    },
    /// As [`Resume::Emulate`], and the kernel then delivers `signal` as the task returns.
    ///
    /// The kernel applies the task's own disposition (handler, default action or ignore), at a
    /// moment the run chose.
    EmulateAndSignal {
        /// What the guest receives.
        ret: i64,
        /// Signal number.
        signal: u32,
    },
    /// Execute a different syscall than the guest asked for.
    Rewrite {
        /// Replacement syscall number.
        nr: u32,
        /// Replacement arguments.
        args: [u64; 6],
    },
    /// Deliver a signal as part of resumption.
    DeliverSignal {
        /// Signal number.
        signal: u32,
    },
    /// End the task.
    Terminate {
        /// The signal it died of, if any.
        ///
        /// Only the world knows it for a task that asked to die (`abort`, an aborting panic);
        /// without it the task would be recorded as a clean exit.
        signal: Option<u32>,
    },
}

/// What a backend measures progress in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgressUnit {
    /// Retired conditional branches, read from a hardware counter.
    Branches,
    /// Syscalls, used where a hardware counter is unavailable.
    Syscalls,
    /// Scripted operations, for backends that execute a synthetic guest.
    Operations,
}

/// What an implementation can and cannot do.
///
/// Read once at startup and recorded in the bundle. A missing capability the run needed marks the
/// run degraded; a degraded run is never reported clean.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BackendCapabilities {
    /// Short stable name, recorded in the bundle.
    pub name: &'static str,
    /// Whether timestamp counter reads are trapped.
    pub traps_rdtsc: bool,
    /// Whether processor identification is trapped.
    pub traps_cpuid: bool,
    /// Whether hardware random instructions are trapped.
    pub traps_rdrand: bool,
    /// What progress is counted in.
    pub progress_unit: ProgressUnit,
    /// Whether a stopped guest can be released to an external debugger.
    pub hands_over_to_debugger: bool,
    /// Largest number of concurrent tasks.
    pub max_tasks: u32,
}

/// The x86-64 register file, as far as interception needs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Regs {
    /// Instruction pointer.
    pub rip: u64,
    /// Return value and syscall number register.
    pub rax: u64,
    /// Argument registers, in syscall ABI order.
    pub args: [u64; 6],
    /// Second half of a 64-bit timestamp counter result.
    pub rdx: u64,
    /// Sub-leaf selector for processor identification.
    pub rcx: u64,
    /// Remaining identification result registers.
    pub rbx: u64,
}

/// One mapped region of a guest address space.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mapping {
    /// First address.
    pub start: GuestAddr,
    /// One past the last address.
    pub end: GuestAddr,
    /// Whether the region is executable, and so scanned for untrappable instructions.
    pub executable: bool,
    /// Whether the region is writable.
    pub writable: bool,
    /// Backing path, where the region has one.
    pub path: Option<String>,
}

/// Failures that originate in a backend.
///
/// One code per condition. `BOHR-3005` is reserved for a guest state the backend cannot describe;
/// ordinary mistakes have their own codes.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// `BOHR-1001` — the program cannot be executed.
    #[error("BOHR-1001: cannot start guest {program}: {reason}")]
    CannotStart {
        /// What was asked for.
        program: String,
        /// Why it failed.
        reason: String,
    },

    /// `BOHR-3007` — an operation named a task that does not exist or is not stopped.
    #[error("BOHR-3007: {task} is not a stopped task")]
    NoSuchTask {
        /// The task named.
        task: TaskId,
    },

    /// `BOHR-3008` — a memory access fell outside the guest's mappings.
    #[error("BOHR-3008: guest memory access at {addr} of {len} bytes is out of range")]
    BadAddress {
        /// Where the access started.
        addr: GuestAddr,
        /// How many bytes were requested.
        len: usize,
    },

    /// `BOHR-3009` — the resumption made no sense for the stop being resumed from.
    #[error("BOHR-3009: cannot apply {action} at this stop")]
    InvalidResume {
        /// What was attempted.
        action: String,
    },

    /// `BOHR-3005` — the guest reached a state the backend cannot describe.
    #[error("BOHR-3005: backend lost track of the guest: {0}")]
    Lost(String),

    /// `BOHR-3011` — the guest a caller supplied is not one that can be executed.
    ///
    /// A scripted guest with a jump past its end or a bit index outside a byte is a broken test,
    /// and is reported instead of normalised.
    #[error("BOHR-3011: the guest given to this backend is malformed: {reason}")]
    BadProgram {
        /// What about it cannot be executed.
        reason: String,
    },

    /// `BOHR-3010` — the host refused something the backend needed to do its work.
    ///
    /// Built with [`BackendError::host`], which makes the caller state what it was doing; there is
    /// no `#[from]` conversion.
    #[error("BOHR-3010: {doing}: {source}")]
    Host {
        /// What the backend was attempting.
        doing: String,
        /// What the host said.
        source: std::io::Error,
    },
}

impl BackendError {
    /// Names a host failure with what the backend was doing when it happened.
    #[must_use]
    pub fn host(doing: impl Into<String>, source: std::io::Error) -> Self {
        Self::Host { doing: doing.into(), source }
    }
}
