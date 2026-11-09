//! The backend boundary: guest execution, stated in terms of what a guest does.
//!
//! The whole vocabulary is guest operations — a task stopped at a syscall, a task resumed with a
//! substituted return value, memory read at a guest address. No host mechanism appears in it, and
//! no layer above may branch on which implementation is in use. That is what allows the mechanism
//! to be replaced without touching the layers that give the tool its value.
//! See `docs/ARCHITECTURE.md` §5.2 and `docs/API.md` §3.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod code;

mod coverage;

use bohr_types::{GuestAddr, NodeId, TaskId};
pub use coverage::{CoverageMap, DEFAULT_COVERAGE_MAP_BYTES};

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
    /// At most one task is ever executing. `None` means every task has ended, which is a normal
    /// end of run rather than a failure.
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
    /// Nothing more is reported about the task afterwards, its exit included: it is forgotten
    /// here, and the caller takes it off its own books when it asks.
    fn kill(&mut self, task: TaskId) -> Result<(), BackendError>;

    /// What the guest executed, where this implementation can observe it.
    ///
    /// Coverage is a fact about guest execution, which is what this boundary is for, so it is
    /// reported here rather than gathered by whoever wanted it. `None` means this build has no way
    /// to observe execution at all; a map that is present and empty means the guest ran without
    /// the runtime that fills one — see [`CoverageMap::is_empty`].
    ///
    /// Read after a run rather than during it, because a search that sampled mid-run would depend
    /// on when it sampled. See `docs/API.md` §5.5.
    ///
    /// Takes the backend by mutable reference because reading is an act: the counters live in
    /// memory the guest writes, and this is where they are copied out of it.
    fn coverage(&mut self) -> Option<&CoverageMap> {
        None
    }

    /// How many edges the guest was instrumented with, where the guest says.
    ///
    /// **The number that decides whether the map is big enough**, and the guest is the only side
    /// that knows it: a map with fewer counters than the guest has edges puts two edges on one
    /// counter, and two paths that differ only there are one path to everything downstream. Left
    /// unsaid, a map too small looks exactly like a guest with a great many paths, and the search
    /// degrades without saying so.
    ///
    /// Nought where nothing said, which is every guest without the coverage runtime.
    fn coverage_edges(&self) -> u64 {
        0
    }

    /// Releases every stopped guest to an external debugger, and says how to reach each one.
    ///
    /// Guarded by [`BackendCapabilities::hands_over_to_debugger`]. The default is that there is
    /// nothing to hand over: a backend that executes a synthetic guest has no process for a
    /// debugger to attach to, and an empty list is not the same claim as being unable to.
    ///
    /// **This ends the run.** Afterwards the guest is no longer under this backend's control —
    /// nothing may be resumed, read, written or stopped through it — and dropping the backend is
    /// what ends the guest. Whoever calls this is the last thing that knows how to.
    ///
    /// # Errors
    ///
    /// Reports what stopped the hand-over. A backend that fails part-way has released whichever
    /// guests it got to, which is why the failure names one rather than being silent.
    fn hand_over(&mut self) -> Result<Vec<DebuggerTarget>, BackendError> {
        Ok(Vec::new())
    }

    /// Holds every guest as it stands, so the run can later go on from here more than once.
    ///
    /// Taken with every task stopped, between two picks. `Ok(None)` from a backend that cannot,
    /// which [`BackendCapabilities::checkpoints`] says in advance.
    fn checkpoint(&mut self) -> Result<Option<CheckpointId>, BackendError> {
        Ok(None)
    }

    /// Puts every guest back as it stood at `id`, keeping the checkpoint for another restore.
    ///
    /// `Ok(false)` from a backend that cannot, or for an id it does not hold.
    fn restore(&mut self, _id: CheckpointId) -> Result<bool, BackendError> {
        Ok(false)
    }
}

/// Where an external debugger is told to look, and what it will not be able to do there.
///
/// Opaque by construction. What identifies a stopped guest to a debugger is the backend's own
/// business, and a process identifier here would be a host mechanism sitting above a boundary that
/// exists to keep host mechanisms below it. What crosses is a command a person can run and a list
/// of the things that will not work once they do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DebuggerTarget {
    node: NodeId,
    command: String,
    limits: Vec<String>,
}

impl DebuggerTarget {
    /// A target a debugger can be pointed at.
    ///
    /// The command must be runnable exactly as written: whoever reads it is looking at a frozen
    /// process and a terminal, and is in no position to work out what was meant.
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

/// What to execute, and in what environment.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct GuestSpec {
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Environment, in the order it is presented to the guest.
    ///
    /// Built from configuration rather than inherited: an inherited environment carries host
    /// details such as `PWD` and `HOSTNAME` straight into the run.
    pub env: Vec<(String, String)>,
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
    /// The normal path. A syscall reaching the host kernel is an exception that has to be
    /// justified per syscall, because the guest is meant to be talking to the world, not to Linux.
    Emulate {
        /// What the guest receives.
        ret: i64,
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
        /// The signal it died of, where it died of one rather than reaching an exit.
        ///
        /// Carried because only the world knows: a task that stops *on* a signal is one the kernel
        /// can see for itself, and a task that asks to die — `abort`, a failed assertion, a panic
        /// that aborts — stops on nothing at all. Without this it is recorded as having exited
        /// cleanly, and a guest that killed itself reads as a run that passed.
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
/// Read once at startup and recorded in the bundle. A `false` where the run needed `true` marks
/// the run degraded rather than failing it; what is never done is reporting a degraded run clean.
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
    /// Whether every guest can be held at a point and resumed from it again
    /// ([`DeterminismBackend::checkpoint`]).
    pub checkpoints: bool,
}

/// A held point of a whole run's guests, named by the backend that holds it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CheckpointId(pub u64);

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
    /// Whether the region is executable, and therefore worth scanning for untrappable
    /// instructions.
    pub executable: bool,
    /// Whether the region is writable.
    pub writable: bool,
    /// Backing path, where the region has one.
    pub path: Option<String>,
}

/// Failures that originate in a backend.
///
/// One code to one condition. `BOHR-3005` means the guest died in a way the backend cannot
/// describe -- our bug or a hostile guest -- so an ordinary mistake (a task that already exited, a
/// read off the end of a mapping, a host descriptor running out) never wears it.
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
    /// A scripted guest is written by whoever wrote the test, so a jump past the end of it or a
    /// bit index outside a byte is a mistake in the test rather than anything the guest did.
    /// Normalising it — treating the jump as the end of the program, masking the bit into range —
    /// turns a broken test into a passing one.
    #[error("BOHR-3011: the guest given to this backend is malformed: {reason}")]
    BadProgram {
        /// What about it cannot be executed.
        reason: String,
    },

    /// `BOHR-3010` — the host refused something the backend needed to do its work.
    ///
    /// Not a `#[from]`: a conversion that happens by writing `?` is one nobody decided on, and it
    /// turned every host failure — a descriptor limit, a full disk, a read that came up short —
    /// into whichever error the surrounding function happened to return. What the backend was
    /// doing when the host refused is the useful half, so the caller states it.
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
