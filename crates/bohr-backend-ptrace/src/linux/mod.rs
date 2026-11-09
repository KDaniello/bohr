//! Syscall-level interception on Linux x86-64.
//!
//! The only crate that touches the host directly, and the only one allowed `unsafe`. Everything it
//! learns about the guest is published through the backend boundary, so nothing above needs to know
//! that any of this exists.

mod coverage;
pub mod doctor;
mod mem;
mod spawn;
mod syscalls;

use std::collections::BTreeMap;

use bohr_backend::{
    BackendCapabilities, BackendError, CoverageMap, DebuggerTarget, DeterminismBackend, GuestExit,
    GuestSpec, Mapping, ProgressUnit, Regs, Resume, Stop, StopKind,
};
use bohr_types::{GuestAddr, NodeId, TaskId};

/// Sets the processor's identification instruction to fault. Absent on platforms that do not expose
/// the underlying register, which is reported rather than worked around.
const ARCH_SET_CPUID: libc::c_int = 0x1012;

/// The event a group-stop arrives as under `PTRACE_SEIZE`. Absent from `libc`.
const PTRACE_EVENT_STOP: libc::c_int = 128;

/// The bit an x32 syscall number carries, and the code segment a 64-bit one runs in.
const X32_SYSCALL_BIT: u64 = 0x4000_0000;
const CS_64_BIT: u64 = 0x33;

/// Which foreign ABI a syscall arrived through, where it did not arrive through this one.
///
/// Two ways in, both cheap to see: an i386 call runs in the compatibility code segment, and an x32
/// call carries a bit in its number. Neither is a number this build's table means, and answering
/// one as though it were is how a guest makes the world model and the supervisor disagree.
fn foreign_syscall_abi(regs: &libc::user_regs_struct) -> Option<&'static str> {
    if regs.orig_rax & X32_SYSCALL_BIT != 0 {
        return Some("x32");
    }
    (regs.cs != CS_64_BIT).then_some("i386")
}

/// State of one traced task between stops.
#[derive(Debug)]
struct Task {
    pid: libc::pid_t,
    /// Thread group the task belongs to, which is the guest process a node maps onto.
    group: libc::pid_t,
    /// The node this task belongs to, and the program it is running.
    ///
    /// Kept because a hand-over prints one line per process and a bare number tells the reader
    /// nothing about which of their nodes they are about to attach to.
    node: NodeId,
    program: String,
    /// Return value owed to a syscall that was skipped, applied at its exit stop.
    pending_return: Option<i64>,
    /// Whether the exit of the syscall now running is owed to the world as a stop of its own.
    ///
    /// Set when the world replaced a call rather than answering it: a replacement is made because
    /// the world wants what the host did with it, and the answer exists only on the way out.
    reporting_exit: bool,
    /// Bytes to step the instruction pointer past on the next resume, for an instruction the
    /// supervisor answered instead of the processor.
    pending_skip: Option<u64>,
    /// Registers as of the current stop. A stopped task cannot change them, so reading them once
    /// serves every caller until it runs again; resuming drops the cache.
    regs: Option<libc::user_regs_struct>,
    /// Whether the task has been seen stopped at least once. A task created by `clone` is stopped
    /// by the kernel before its first instruction, and that stop is machinery rather than an
    /// event.
    attached: bool,
    /// Whether the task is in a job-control stop rather than an ordinary one.
    ///
    /// A guest that stopped itself is waiting to be continued by a signal, not by us. Restarting
    /// it with `PTRACE_CONT` runs a task the guest asked to keep stopped; `PTRACE_LISTEN` leaves
    /// it stopped and lets the supervisor hear about it when something changes.
    group_stopped: bool,
}

impl Task {
    fn new(pid: libc::pid_t, group: libc::pid_t, node: NodeId, program: String) -> Self {
        Self {
            pid,
            group,
            node,
            program,
            pending_return: None,
            reporting_exit: false,
            pending_skip: None,
            regs: None,
            attached: false,
            group_stopped: false,
        }
    }
}

/// Executes a real program under `ptrace`, with an in-kernel pre-filter.
///
/// Every task the guest creates is traced and stopped; exactly one is ever let run, which is what
/// makes an interleaving a decision the scheduler takes rather than one the host takes. See
/// `docs/ARCHITECTURE.md` §5.4.
#[derive(Debug)]
pub struct PtraceBackend {
    tasks: BTreeMap<TaskId, Task>,
    /// The reverse of `tasks`, since a wait reports a host pid and everything above speaks in task
    /// identifiers.
    by_pid: BTreeMap<libc::pid_t, TaskId>,
    /// The task that was last let run, and so the only one that can be executing.
    current: Option<TaskId>,
    next_task: u32,
    /// Whether this host exposes the register that makes the identification instruction fault.
    traps_cpuid_available: bool,
    /// Whether the guest that is running actually had it turned on.
    ///
    /// **Separate from the host's answer, because they were read as one and they are not.** The
    /// host probe sets the flag in a throwaway child; the guest is a different task, and `execve`
    /// clears the setting on the way in (`arch_setup_new_exec` re-enables `CPUID` for a task that
    /// has just exec'd) — so a capable host said nothing about a guest that was never given it,
    /// and a run reported `determinism: full` while the guest's `CPUID` executed on the bare
    /// processor.
    ///
    /// **Always false, and that is the honest answer.** Setting it needs the guest to make the
    /// call itself — there is no `ptrace` request that sets it for another task — which means
    /// injecting `arch_prctl` at the post-exec stop: borrow the word at the entry point through
    /// `PTRACE_POKEDATA` (the bulk interface will not write a read-only text page), point the
    /// registers at the call, single-step, put everything back. That is written down here rather
    /// than written, because no host available to this project can trap `CPUID` at all, and an
    /// injection whose success cannot be observed reports success on a step that did not run —
    /// which is the same lie as before, told with more confidence. It lands with a host that
    /// `bohr doctor` passes.
    cpuid_trapped: bool,
    isolated: bool,
    /// Whether the guests have been released to a debugger, which moves the responsibility for
    /// ending them from the kernel to this process.
    handed_over: bool,
    notes: Vec<String>,
    /// Where the guest's coverage runtime writes, and where it is read from once a run has ended.
    /// One channel for every node: what a campaign wants is what the run covered, not what each
    /// process covered separately.
    coverage: Option<coverage::CoverageChannel>,
    /// The counters, copied out of that channel when somebody asks for them.
    covered: CoverageMap,
}

impl Default for PtraceBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PtraceBackend {
    /// A backend that has probed what this host can do, with a coverage map of the default size.
    #[must_use]
    pub fn new() -> Self {
        Self::with_coverage_bytes(bohr_backend::DEFAULT_COVERAGE_MAP_BYTES)
    }

    /// The same, with a coverage map of `bytes` counters.
    ///
    /// Sized by the caller because only the caller knows what it is hunting. See
    /// [`bohr_backend::DeterminismBackend::coverage_edges`] for the number to size it from.
    #[must_use]
    pub fn with_coverage_bytes(bytes: usize) -> Self {
        Self {
            tasks: BTreeMap::new(),
            by_pid: BTreeMap::new(),
            current: None,
            next_task: 0,
            // What the host can do, which is a different question from what the guest was given.
            // The guest's own answer is recorded per spawn in `cpuid_trapped`.
            traps_cpuid_available: cpuid_faulting_available(),
            cpuid_trapped: false,
            isolated: namespaces_available(),
            handed_over: false,
            notes: Vec::new(),
            coverage: coverage::CoverageChannel::open(bytes),
            covered: CoverageMap::with_bytes(bytes),
        }
    }

    /// Conditions this host imposed that the run should be recorded as degraded for.
    #[must_use]
    pub fn degradations(&self) -> &[String] {
        &self.notes
    }

    /// Records that a processor question reached this side and was answered.
    ///
    /// A run whose guest asks and is answered is not degraded for the host's inability to make the
    /// instruction fault, whichever way the fault was arranged. The note is taken back rather than
    /// never written, because at the moment it is written nothing here knows yet.
    fn answered_cpu_identification(&mut self) {
        if self.cpuid_trapped {
            return;
        }
        self.cpuid_trapped = true;
        self.notes.retain(|note| note != "cpuid_not_trapped" && note != "cpuid_not_trappable");
    }

    /// Adds a traced task and gives it the identifier everything above uses.
    fn register(
        &mut self,
        pid: libc::pid_t,
        group: libc::pid_t,
        node: NodeId,
        program: String,
    ) -> TaskId {
        let id = TaskId(self.next_task);
        self.next_task += 1;
        self.tasks.insert(id, Task::new(pid, group, node, program));
        self.by_pid.insert(pid, id);
        id
    }

    /// Which node and program a thread group belongs to, for a task that arrived unannounced.
    ///
    /// A thread reports before the event that creates it in some orderings, so it is adopted from
    /// whatever else is in its group. Node zero is the fallback, which is what a single-node run
    /// has anyway and is better than refusing to adopt a task the kernel has already produced.
    fn whose_group(&self, pid: libc::pid_t) -> (NodeId, String) {
        self.tasks
            .values()
            .find(|task| task.group == pid)
            .map_or_else(|| (NodeId::ZERO, String::new()), |task| (task.node, task.program.clone()))
    }

    fn task(&self, id: TaskId) -> Result<&Task, BackendError> {
        self.tasks.get(&id).ok_or(BackendError::NoSuchTask { task: id })
    }

    fn task_mut(&mut self, id: TaskId) -> Result<&mut Task, BackendError> {
        self.tasks.get_mut(&id).ok_or(BackendError::NoSuchTask { task: id })
    }

    /// Drops a task that has ended, so nothing waits on it and the host may reuse its pid.
    ///
    /// Removed rather than marked: a guest that creates threads in a loop would otherwise leave one
    /// entry behind per thread for the length of the run.
    fn forget(&mut self, id: TaskId) {
        if let Some(task) = self.tasks.remove(&id) {
            self.by_pid.remove(&task.pid);
        }
        if self.current == Some(id) {
            self.current = None;
        }
    }

    /// Takes over a new task that is already stopped, attaching the options it did not inherit.
    fn settle(&mut self, id: TaskId) -> Result<(), BackendError> {
        let pid = self.task(id)?.pid;
        if set_options(pid).is_err() {
            // Between stopping and being taken over the task can be killed, by its own group
            // exiting. Nothing above has heard of it, so there is nothing to report.
            self.forget(id);
            return Ok(());
        }
        self.task_mut(id)?.attached = true;
        Ok(())
    }

    /// Waits for a new task to reach the stop the kernel puts it in, then takes it over.
    ///
    /// A task cannot be resumed before that stop has been collected, and the run loop may pick it
    /// as soon as it hears the task exists.
    fn await_first_stop(&mut self, id: TaskId) -> Result<(), BackendError> {
        let pid = self.task(id)?.pid;
        let mut status = 0;
        // SAFETY: `pid` is a task the kernel just created under our tracing; `status` is ours.
        if unsafe { libc::waitpid(pid, &mut status, libc::__WALL) } < 0 {
            return Err(BackendError::Lost(format!(
                "waiting for a new task: {}",
                spawn::last_error()
            )));
        }
        self.settle(id)
    }

    /// Registers of a stopped task, read at most once per stop.
    fn cached_regs(&mut self, id: TaskId) -> Result<libc::user_regs_struct, BackendError> {
        let task = self.task_mut(id)?;
        if let Some(regs) = task.regs {
            return Ok(regs);
        }
        let regs = get_regs(task.pid)?;
        self.task_mut(id)?.regs = Some(regs);
        Ok(regs)
    }

    /// Writes registers and keeps the cache in step with what the task now holds.
    fn store_regs(
        &mut self,
        id: TaskId,
        regs: &libc::user_regs_struct,
    ) -> Result<(), BackendError> {
        let task = self.task_mut(id)?;
        set_regs(task.pid, regs)?;
        task.regs = Some(*regs);
        Ok(())
    }

    /// Hands the task back to the kernel, after which nothing observed at the stop still holds.
    ///
    /// A failure here means the task is gone or was never ours. Ignoring it would leave the loop
    /// waiting for a stop that can no longer arrive, which reads as a hang rather than an error.
    fn release(
        &mut self,
        id: TaskId,
        request: libc::c_uint,
        signal: libc::c_long,
    ) -> Result<(), BackendError> {
        let task = self.task_mut(id)?;
        task.regs = None;
        let pid = task.pid;
        // A job-control stop is left as one. The guest is stopped because it asked to be, and the
        // request that resumes an ordinary stop would take that away from it.
        let request = if std::mem::take(&mut task.group_stopped) {
            libc::PTRACE_LISTEN as libc::c_uint
        } else {
            request
        };
        // SAFETY: the task is stopped and owned by us; the request passes no pointer.
        let rc = unsafe { libc::ptrace(request, pid, 0, signal) };
        if rc < 0 {
            return Err(BackendError::Lost(format!("resuming {id}: {}", spawn::last_error())));
        }
        self.current = Some(id);
        Ok(())
    }
}

impl DeterminismBackend for PtraceBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "ptrace",
            traps_rdtsc: true,
            traps_cpuid: self.cpuid_trapped,
            // No mechanism exists to trap the hardware random instructions; they are detected by
            // scanning instead, and a run where one survives is reported degraded.
            traps_rdrand: false,
            progress_unit: ProgressUnit::Syscalls,
            hands_over_to_debugger: true,
            max_tasks: 4096,
            checkpoints: false,
        }
    }

    fn spawn(&mut self, node: NodeId, spec: &GuestSpec) -> Result<TaskId, BackendError> {
        let pid = spawn::spawn_traced(spec, self.isolated, self.coverage.as_ref().map(|c| c.fd()))?;

        // Run to the exec, where the stack still carries the auxiliary vector. The guest is already
        // traced, and its own `execve` is filtered like any other syscall, so a seccomp event
        // arrives first: nothing above the boundary should see the machinery of starting up.
        let mut status = 0;
        loop {
            // SAFETY: `pid` is the guest we attached to and `status` is a local we own.
            if unsafe { libc::waitpid(pid, &mut status, libc::__WALL) } < 0 {
                return Err(BackendError::Lost(format!(
                    "waiting for exec: {}",
                    spawn::last_error()
                )));
            }
            if is_event(status, libc::PTRACE_EVENT_EXEC) {
                break;
            }
            if !libc::WIFSTOPPED(status) {
                return Err(BackendError::CannotStart {
                    program: spec.argv.first().cloned().unwrap_or_default(),
                    reason: describe_status(status),
                });
            }
            // SAFETY: the task is stopped and owned by us.
            if unsafe { libc::ptrace(libc::PTRACE_CONT, pid, 0, 0) } < 0 {
                return Err(BackendError::CannotStart {
                    program: spec.argv.first().cloned().unwrap_or_default(),
                    reason: format!("resuming the guest: {}", spawn::last_error()),
                });
            }
        }

        let regs = get_regs(pid)?;
        let report = spawn::neutralise_auxv(pid, regs.rsp)?;
        if !report.vdso_disabled {
            self.notes.push("vdso_absent_from_auxv".to_owned());
        }
        if !report.at_random_zeroed {
            self.notes.push("at_random_absent_from_auxv".to_owned());
        }
        if !self.cpuid_trapped {
            // Named for what it is: this run's guest was not trapped, whatever the host could
            // have done. Pushed on the guest's answer rather than the host's, because the host's
            // left a capable machine silent and the run then read as fully deterministic.
            self.notes.push(if self.traps_cpuid_available {
                "cpuid_not_trapped".to_owned()
            } else {
                "cpuid_not_trappable".to_owned()
            });
        }
        if !self.isolated {
            self.notes.push("namespaces_unavailable".to_owned());
        }

        let id = self.register(pid, pid, node, spec.argv.first().cloned().unwrap_or_default());
        self.task_mut(id)?.attached = true;
        Ok(id)
    }

    fn next_stop(&mut self) -> Result<Option<Stop>, BackendError> {
        loop {
            if self.current.is_none() {
                return Ok(None);
            }

            let mut status = 0;
            // Any traced task may report, not only the one that was resumed: a thread created by
            // `clone` stops on its own before its first instruction. `__WALL` covers threads, whose
            // stops the default wait does not report.
            // SAFETY: `status` is a local we own.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL) };
            if pid < 0 {
                return Err(BackendError::Lost(format!(
                    "waiting for any task: {}",
                    spawn::last_error()
                )));
            }

            // A stop from a task the supervisor has not registered is the new task arriving before
            // its creator reported the event. It is adopted and left stopped: the event, when it
            // comes, finds it already known and is what announces it.
            //
            // Unless what arrived is a death, which is the opposite case and has to be told apart
            // first: a task killed with its node is forgotten where the kill happens, and its end
            // reaches here afterwards with nobody left to tell. Adopting it would make a new task
            // out of a pid that no longer exists.
            let Some(id) = self.by_pid.get(&pid).copied() else {
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    continue;
                }
                let (node, program) = self.whose_group(pid);
                let id = self.register(pid, pid, node, program);
                self.settle(id)?;
                continue;
            };

            if libc::WIFEXITED(status) {
                self.forget(id);
                return Ok(Some(Stop {
                    task: id,
                    kind: StopKind::TaskExited {
                        status: GuestExit::Exited(libc::WEXITSTATUS(status)),
                    },
                }));
            }
            if libc::WIFSIGNALED(status) {
                self.forget(id);
                return Ok(Some(Stop {
                    task: id,
                    kind: StopKind::TaskExited {
                        status: GuestExit::Signalled(libc::WTERMSIG(status) as u32),
                    },
                }));
            }
            if !libc::WIFSTOPPED(status) {
                return Err(BackendError::Lost(describe_status(status)));
            }

            // Anything read at the previous stop describes a task that has since run.
            self.task_mut(id)?.regs = None;

            // The already-adopted case, reached when a task stops again before its creator has
            // reported it. Nothing above has heard of it yet, so it waits.
            if !self.task(id)?.attached {
                self.settle(id)?;
                continue;
            }

            // The creation event is the one thing above the boundary hears about a new task. Both
            // it and the new task are left stopped, which is what the run loop is told when it puts
            // the pair in its runnable set.
            if is_event(status, libc::PTRACE_EVENT_CLONE)
                || is_event(status, libc::PTRACE_EVENT_FORK)
                || is_event(status, libc::PTRACE_EVENT_VFORK)
            {
                let shares_memory = is_event(status, libc::PTRACE_EVENT_CLONE);
                let child_pid = i32::try_from(event_message(pid)?).unwrap_or(-1);
                let group = if shares_memory { self.task(id)?.group } else { child_pid };

                let parent = self.task(id)?;
                let (node, program) = (parent.node, parent.program.clone());
                let child = match self.by_pid.get(&child_pid).copied() {
                    // Already here, because it stopped before its creator announced it. What it
                    // was registered with then was a guess made from its own pid and nothing else;
                    // this event carries the truth, and a task left with the guess belongs to the
                    // wrong node and answers to a thread group that is not its own.
                    Some(child) => {
                        let task = self.task_mut(child)?;
                        task.group = group;
                        task.node = node;
                        task.program = program;
                        child
                    }
                    None => self.register(child_pid, group, node, program),
                };
                if !self.task(child)?.attached {
                    self.await_first_stop(child)?;
                }

                self.current = None;
                return Ok(Some(Stop {
                    task: child,
                    kind: StopKind::TaskCreated { parent: id, shares_memory },
                }));
            }

            // A guest that replaces itself starts again behind a fresh auxiliary vector, and the
            // kernel builds that one from scratch: the vDSO comes back and `AT_RANDOM` is filled
            // anew, so the clock stops being intercepted at the exec and the guest reads the host's
            // own. Everything spawn did to the first vector has to be done to this one.
            //
            // Silent above the boundary, for the reason the first exec is: what a task is running
            // is not a scheduling decision, and the run knows the task by the same identity either
            // way. `execve` is admitted by the filter and never reaches the world, so there is no
            // call outstanding to answer here.
            if is_event(status, libc::PTRACE_EVENT_EXEC) {
                let regs = get_regs(pid)?;
                let report = spawn::neutralise_auxv(pid, regs.rsp)?;
                if !report.vdso_disabled {
                    self.notes.push("vdso_absent_from_auxv".to_owned());
                }
                if !report.at_random_zeroed {
                    self.notes.push("at_random_absent_from_auxv".to_owned());
                }
                self.release(id, libc::PTRACE_CONT, 0)?;
                continue;
            }

            if is_event(status, libc::PTRACE_EVENT_SECCOMP) {
                let regs = self.cached_regs(id)?;
                // **The number and the six registers only mean what they say for a call that
                // arrived through the x86-64 ABI.** The filter is built for that architecture and
                // fails a mismatch safe, to a trace rather than to the host — so nothing escapes.
                // What did happen is that an i386 `write`, number 4, reached the world as x86-64
                // number 4, which is `stat`, with i386 arguments read out of the wrong registers:
                // a guest could desync the world model from the supervisor on purpose.
                if let Some(abi) = foreign_syscall_abi(&regs) {
                    return Err(BackendError::Lost(format!(
                        "a syscall arrived through the {abi} ABI, which this build does not read"
                    )));
                }
                self.current = None;
                return Ok(Some(Stop {
                    task: id,
                    kind: StopKind::SyscallEntry {
                        nr: regs.orig_rax as u32,
                        args: [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9],
                    },
                }));
            }

            // A job-control stop, which under `PTRACE_SEIZE` arrives as an event rather than as
            // the signal it looks like. Told apart because it is resumed by a different primitive:
            // `PTRACE_CONT` restarts a guest that asked to stay stopped, and the world was handed
            // a `Signal` for something no signal had been delivered for.
            if is_event(status, PTRACE_EVENT_STOP) {
                self.task_mut(id)?.group_stopped = true;
                self.current = None;
                let kind = StopKind::Signal { signal: libc::WSTOPSIG(status) as u32 };
                return Ok(Some(Stop { task: id, kind }));
            }

            let sig = libc::WSTOPSIG(status);

            // The exit half of a syscall we skipped: the return value the world chose is written
            // here, because the kernel overwrites it on the way out of any syscall.
            if sig == libc::SIGTRAP | 0x80 {
                if let Some(ret) = self.task_mut(id)?.pending_return.take() {
                    let mut regs = self.cached_regs(id)?;
                    regs.rax = ret as u64;
                    self.store_regs(id, &regs)?;
                }
                // A replaced call's answer is the world's business, so it is handed up rather than
                // stepped over. The world sees what the host actually did and gets one chance to
                // act on it before the guest runs again.
                if std::mem::take(&mut self.task_mut(id)?.reporting_exit) {
                    let ret = self.cached_regs(id)?.rax as i64;
                    self.current = None;
                    return Ok(Some(Stop { task: id, kind: StopKind::SyscallExit { ret } }));
                }
                self.release(id, libc::PTRACE_CONT, 0)?;
                continue;
            }

            if sig == libc::SIGSEGV {
                let rip = self.cached_regs(id)?.rip;
                if let Some((leaf, subleaf, width)) = decode_cpu_identification(pid, rip) {
                    // The guest's questions are being answered after all. Said here rather than at
                    // attach because that is where it becomes true: the instruction was rewritten
                    // in a copy of the object before the run began, and nothing on this side knew
                    // it until one of them faulted.
                    self.answered_cpu_identification();
                    self.task_mut(id)?.pending_skip = Some(width);
                    self.current = None;
                    let kind = StopKind::CpuIdentification { leaf, subleaf };
                    return Ok(Some(Stop { task: id, kind }));
                }
                if let Some((rdtscp, width)) = decode_timestamp_read(pid, rip) {
                    self.task_mut(id)?.pending_skip = Some(width);
                    self.current = None;
                    return Ok(Some(Stop {
                        task: id,
                        kind: StopKind::TimestampCounter { rdtscp },
                    }));
                }
            }

            self.current = None;
            return Ok(Some(Stop { task: id, kind: StopKind::Signal { signal: sig as u32 } }));
        }
    }

    fn resume(&mut self, task: TaskId, action: Resume) -> Result<(), BackendError> {
        // An instruction the supervisor answered has to be stepped over however the task leaves
        // this stop. Left outstanding, the debt would be paid against an unrelated instruction the
        // next time the task stopped.
        if let Some(width) = self.task_mut(task)?.pending_skip.take()
            && !matches!(action, Resume::Terminate { .. })
        {
            let mut regs = self.cached_regs(task)?;
            regs.rip = regs.rip.wrapping_add(width);
            self.store_regs(task, &regs)?;
        }

        match action {
            Resume::Continue | Resume::ContinueWithBudget { .. } => {
                self.release(task, libc::PTRACE_CONT, 0)?;
            }
            Resume::DeliverSignal { signal } => {
                self.release(task, libc::PTRACE_CONT, libc::c_long::from(signal))?;
            }
            Resume::Emulate { ret } => {
                // Setting the number to -1 makes the kernel skip the call; the value the world
                // chose is written at the exit stop that follows.
                let mut regs = self.cached_regs(task)?;
                regs.orig_rax = u64::MAX;
                self.store_regs(task, &regs)?;
                self.task_mut(task)?.pending_return = Some(ret);
                self.release(task, libc::PTRACE_SYSCALL, 0)?;
            }
            Resume::Rewrite { nr, args } => {
                let mut regs = self.cached_regs(task)?;
                regs.orig_rax = u64::from(nr);
                regs.rdi = args[0];
                regs.rsi = args[1];
                regs.rdx = args[2];
                regs.r10 = args[3];
                regs.r8 = args[4];
                regs.r9 = args[5];
                self.store_regs(task, &regs)?;
                self.task_mut(task)?.reporting_exit = true;
                self.release(task, libc::PTRACE_SYSCALL, 0)?;
            }
            Resume::Terminate { .. } => {
                self.kill(task)?;
            }
        }
        Ok(())
    }

    fn read_mem(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        out: &mut [u8],
    ) -> Result<(), BackendError> {
        let pid = self.task(task)?.pid;
        mem::read(pid, addr.0, out)
    }

    fn write_mem(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        data: &[u8],
    ) -> Result<(), BackendError> {
        let pid = self.task(task)?.pid;
        mem::write(pid, addr.0, data)
    }

    fn regs(&mut self, task: TaskId) -> Result<Regs, BackendError> {
        let r = self.cached_regs(task)?;
        Ok(Regs {
            rip: r.rip,
            rax: r.rax,
            args: [r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9],
            rdx: r.rdx,
            rcx: r.rcx,
            rbx: r.rbx,
        })
    }

    fn set_regs(&mut self, task: TaskId, regs: &Regs) -> Result<(), BackendError> {
        let mut r = self.cached_regs(task)?;
        r.rip = regs.rip;
        r.rax = regs.rax;
        r.rcx = regs.rcx;
        r.rbx = regs.rbx;
        // Arguments are written back as well. A boundary that hands a field out and then drops it
        // on the way in loses a change without saying so, and `rdx` appears in both spellings, so
        // it is taken from the arguments to keep the two from disagreeing.
        r.rdi = regs.args[0];
        r.rsi = regs.args[1];
        r.rdx = regs.args[2];
        r.r10 = regs.args[3];
        r.r8 = regs.args[4];
        r.r9 = regs.args[5];
        self.store_regs(task, &r)
    }

    fn mappings(&mut self, task: TaskId) -> Result<Vec<Mapping>, BackendError> {
        let pid = self.task(task)?.pid;
        let text = std::fs::read_to_string(format!("/proc/{pid}/maps"))
            .map_err(|e| BackendError::Lost(format!("reading mappings: {e}")))?;
        Ok(text.lines().filter_map(parse_mapping).collect())
    }

    fn kill(&mut self, task: TaskId) -> Result<(), BackendError> {
        let (pid, group) = {
            let t = self.task(task)?;
            (t.pid, t.group)
        };
        // Addressed by thread group and thread, not by number alone: `kill` reads its argument as a
        // process, so a thread identifier would name whichever process happens to have it.
        //
        // **And then let go, because a stopped tracee does not die on being told to.** Every task
        // here is under ptrace and all but one of them are stopped at any moment; a task stopped
        // for its tracer does not act on a pending signal until the tracer releases it, so waiting
        // for it to end would be waiting for something only this call can cause. A node with one
        // task never showed it: the one task was the one running.
        //
        // SAFETY: both identifiers belong to a task we started.
        unsafe {
            libc::syscall(libc::SYS_tgkill, group, pid, libc::SIGKILL);
        }
        // **And then nothing is waited for.** A thread group's leader is not reaped while any of
        // its threads is alive, and all but one of those threads are stopped for this supervisor at
        // any moment — so waiting here for the one task being killed is waiting for the others, and
        // the others are waiting for this supervisor to go round its loop. A node with a single
        // task never showed it, because the one task was the one running.
        //
        // The death arrives at the ordinary wait instead, like every other, and is dropped there:
        // this task is already forgotten and there is nobody left to tell.
        self.forget(task);
        Ok(())
    }

    fn hand_over(&mut self) -> Result<Vec<DebuggerTarget>, BackendError> {
        // One target per thread group, not per task: a debugger attaches to a process and finds
        // its threads through the kernel. A node that forked has more than one group and so more
        // than one line, which is correct and is why each says which node it belongs to.
        let mut groups: BTreeMap<libc::pid_t, (NodeId, String)> = BTreeMap::new();
        for task in self.tasks.values() {
            groups.entry(task.group).or_insert((task.node, task.program.clone()));
        }

        for task in self.tasks.values() {
            // Cleared before anything is detached rather than relied on being dissolved by the
            // detach. If a later step fails, the guests already released must not be killed by
            // this process exiting on the error path.
            //
            // SAFETY: the task is stopped and owned by us; the request passes no pointer.
            unsafe {
                libc::ptrace(
                    libc::PTRACE_SETOPTIONS,
                    task.pid,
                    0,
                    spawn::trace_options() & !libc::PTRACE_O_EXITKILL,
                );
            }
        }

        for task in self.tasks.values() {
            // `PTRACE_DETACH` restarts the tracee, and the only way to leave it stopped for
            // somebody else to attach to is to hand it a stop signal on the way out. The signal is
            // armed first and delivered by the detach.
            // https://man7.org/linux/man-pages/man2/ptrace.2.html
            //
            // SAFETY: both identifiers belong to a task we started, and the task is stopped.
            let released = unsafe {
                libc::syscall(libc::SYS_tgkill, task.group, task.pid, libc::SIGSTOP);
                libc::ptrace(libc::PTRACE_DETACH, task.pid, 0, libc::SIGSTOP)
            };
            if released < 0 {
                return Err(BackendError::Lost(format!(
                    "releasing a guest to a debugger: {}",
                    spawn::last_error()
                )));
            }
        }
        self.handed_over = true;

        Ok(groups
            .into_iter()
            .map(|(pid, (node, program))| {
                DebuggerTarget::new(
                    node,
                    format!("gdb -p {pid} {program}").trim().to_owned(),
                    limits(),
                )
            })
            .collect())
    }

    fn coverage(&mut self) -> Option<&CoverageMap> {
        let channel = self.coverage.as_ref()?;
        channel.read_into(self.covered.as_bytes_mut());
        Some(&self.covered)
    }

    fn coverage_edges(&self) -> u64 {
        self.coverage.as_ref().map_or(0, coverage::CoverageChannel::guards)
    }
}

/// What a session with a released guest cannot do, said once and printed with every target.
///
/// Not decoration. The filter this guest was started under traces by default, and a debugger does
/// not set the option that tells the kernel a tracer is listening for those traps — so a syscall
/// under it does not execute, it returns as though the call did not exist. Somebody told only
/// where to attach will find that out by running `continue` and reading nonsense.
fn limits() -> Vec<String> {
    vec![
        "the guest cannot be continued: every syscall it attempts returns as though the call did \
         not exist, because the filter it was started under expects a supervisor that is no \
         longer there"
            .to_owned(),
        "reading the clock faults, and so does asking the processor to identify itself: both were \
         set to trap and stay that way after the hand-over"
            .to_owned(),
        "the world it was running in — its files, its sockets, its peers — was this process's \
         bookkeeping and is gone; a debugger sees a process with almost no operating system state"
            .to_owned(),
        "determinism ends here: nothing done from now on belongs to the run that was reproduced"
            .to_owned(),
    ]
}

/// Ends guests that were released to a debugger.
///
/// Until a hand-over the kernel does this: every task carries an option that ties its life to the
/// supervisor's. Releasing it clears that option so the guest outlives the detach, which leaves
/// this as the only thing that can end it — and the guest is not our child, so nothing else will.
impl Drop for PtraceBackend {
    fn drop(&mut self) {
        if !self.handed_over {
            return;
        }
        for task in self.tasks.values() {
            // SAFETY: both identifiers belong to a task we started. The process may already be
            // gone, in which case the signal is refused and nothing else happens.
            unsafe { libc::syscall(libc::SYS_tgkill, task.group, task.pid, libc::SIGKILL) };
        }
    }
}

/// Trace options every task carries. A task created by `clone` inherits none of them, so they are
/// attached at the stop that hands it over.
fn set_options(pid: libc::pid_t) -> Result<(), BackendError> {
    // SAFETY: the task is stopped, which is when options may be set.
    let rc = unsafe { libc::ptrace(libc::PTRACE_SETOPTIONS, pid, 0, spawn::trace_options()) };
    if rc < 0 {
        return Err(BackendError::Lost(format!("setting trace options: {}", spawn::last_error())));
    }
    Ok(())
}

/// The value a `PTRACE_EVENT_*` stop carries, which for a creation event is the new host pid.
fn event_message(pid: libc::pid_t) -> Result<libc::c_ulong, BackendError> {
    let mut message: libc::c_ulong = 0;
    // SAFETY: the task is stopped at an event; `message` is a local we own.
    let rc =
        unsafe { libc::ptrace(libc::PTRACE_GETEVENTMSG, pid, 0, std::ptr::from_mut(&mut message)) };
    if rc < 0 {
        return Err(BackendError::Lost(format!("reading an event: {}", spawn::last_error())));
    }
    Ok(message)
}

/// Reads the instruction at the fault address and reports whether it is a processor
/// identification, and which leaf it asked for.
///
/// `ARCH_SET_CPUID` turns the instruction into a general protection fault, which reaches the
/// supervisor as `SIGSEGV` — the same signal a timestamp read arrives as, so the two are told
/// apart by the bytes at the fault address. Without this the fault fell through to the guest as an
/// ordinary segmentation fault, and the boundary's `CpuIdentification` was a variant nothing built.
fn decode_cpu_identification(pid: libc::pid_t, rip: u64) -> Option<(u32, u32, u64)> {
    let mut code = [0u8; 2];
    if mem::read(pid, rip, &mut code).is_err() {
        return None;
    }
    // Either the instruction itself, where the processor was made to fault on it, or the byte that
    // stands in for it where it had to be rewritten instead. Both mean the same question, and the
    // stand-in is recognised here rather than from a table because the program a guest runs is
    // mapped by the kernel: nothing on this side knows where its offsets landed.
    if code != [0x0F, 0xA2] && code[0] != bohr_backend::code::cpuid::TRAP[0] {
        return None;
    }
    // The leaf is in `eax` and the sub-leaf in `ecx`, which is where the guest put them.
    let regs = get_regs(pid).ok()?;
    Some((regs.rax as u32, regs.rcx as u32, 2))
}

/// Reads the instruction at the fault address and reports whether it is a timestamp counter read.
///
/// `PR_SET_TSC` turns those reads into a general protection fault, which reaches the supervisor as
/// `SIGSEGV`; every other cause of that signal is a real fault and belongs to the guest.
fn decode_timestamp_read(pid: libc::pid_t, rip: u64) -> Option<(bool, u64)> {
    let mut code = [0u8; 3];
    if mem::read(pid, rip, &mut code).is_err() {
        return None;
    }
    match code {
        [0x0F, 0x31, _] => Some((false, 2)),
        [0x0F, 0x01, 0xF9] => Some((true, 3)),
        _ => None,
    }
}

/// Whether this host lets an unprivileged process create the namespaces a guest is confined to.
///
/// Answered once and reused, because the answer is a property of the host and creating a network
/// namespace is not cheap enough to ask a thousand times.
fn namespaces_available() -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| {
        // The whole sequence a launch performs, not its first step. `unshare` succeeding says
        // nothing about the identifier maps, which an unprivileged process is allowed to write
        // exactly once and under conditions this probe cannot reason about from outside -- and a
        // host that grants the one and refuses the other sent the run to a refusal `spawn` had
        // already recorded as impossible.
        probe_in_child(|| {
            // SAFETY: between `fork` and `_exit` in a child that performs nothing else, which is
            // the state `enter_namespaces` requires.
            unsafe { spawn::enter_namespaces() }
        })
    })
}

/// Whether this host can make the identification instruction fault.
///
/// Probed in a child for the same reason as the namespaces, and because the setting is per task:
/// applied to the supervisor it would make the supervisor own `CPUID` fault while the probe ran.
fn cpuid_faulting_available() -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| {
        probe_in_child(|| {
            // SAFETY: the probe only sets the flag, in a child that exits immediately.
            unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) == 0 }
        })
    })
}

/// Runs a host capability probe in a throwaway child and reports whether it succeeded.
///
/// A probe that changes task state is asked this way so the supervisor never carries the change.
fn probe_in_child(probe: impl FnOnce() -> bool) -> bool {
    // SAFETY: fork is async-signal-safe; the child performs the probe and never returns.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return false;
        }
        if pid == 0 {
            libc::_exit(i32::from(!probe()));
        }
        let mut status = 0;
        if libc::waitpid(pid, &mut status, 0) < 0 {
            return false;
        }
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }
}

fn get_regs(pid: libc::pid_t) -> Result<libc::user_regs_struct, BackendError> {
    // SAFETY: the struct is plain data and is fully written by the kernel on success.
    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    // SAFETY: the task is stopped; `regs` is ours and large enough.
    let rc = unsafe { libc::ptrace(libc::PTRACE_GETREGS, pid, 0, std::ptr::from_mut(&mut regs)) };
    if rc < 0 {
        return Err(BackendError::Lost(format!("reading registers: {}", spawn::last_error())));
    }
    Ok(regs)
}

fn set_regs(pid: libc::pid_t, regs: &libc::user_regs_struct) -> Result<(), BackendError> {
    // SAFETY: the task is stopped; `regs` is ours and is only read by the kernel.
    let rc = unsafe { libc::ptrace(libc::PTRACE_SETREGS, pid, 0, std::ptr::from_ref(regs)) };
    if rc < 0 {
        return Err(BackendError::Lost(format!("writing registers: {}", spawn::last_error())));
    }
    Ok(())
}

fn is_event(status: libc::c_int, event: libc::c_int) -> bool {
    libc::WIFSTOPPED(status)
        && libc::WSTOPSIG(status) == libc::SIGTRAP
        && (status >> 16) & 0xff == event
}

fn describe_status(status: libc::c_int) -> String {
    if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        if let Some(step) = spawn::setup_failure(code) {
            return step.to_owned();
        }
        return format!("the guest exited with {code} before it could be traced");
    }
    format!("unexpected wait status {status:#x}")
}

fn parse_mapping(line: &str) -> Option<Mapping> {
    let mut parts = line.split_whitespace();
    let range = parts.next()?;
    let perms = parts.next()?;
    let (start, end) = range.split_once('-')?;
    Some(Mapping {
        start: GuestAddr(u64::from_str_radix(start, 16).ok()?),
        end: GuestAddr(u64::from_str_radix(end, 16).ok()?),
        executable: perms.contains('x'),
        writable: perms.contains('w'),
        path: parts.nth(3).map(str::to_owned),
    })
}

/// Every syscall this build has an opinion about, for a diagnosis to compare a target against.
pub fn syscall_table() -> impl Iterator<Item = (u32, &'static str)> {
    syscalls::TABLE.iter().map(|e| (e.nr as u32, e.name))
}

/// The name of a syscall, where this build knows one.
pub fn syscall_name(nr: u32) -> Option<&'static str> {
    syscalls::name_of(nr)
}

/// The name of a syscall this build does not model, where one is known.
///
/// For a survey's report and nothing else: what it names is by definition absent from the table,
/// so an answer here is a courtesy to a reader rather than an input to any decision.
pub fn unmodelled_syscall_name(nr: u32) -> Option<&'static str> {
    syscalls::name_of_unmodelled(nr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe that measures less than the operation is a probe that lies, and this one lied in
    /// the permissive direction: it tested `unshare` alone, while a launch also denies
    /// `setgroups`, writes both identifier maps and names the machine. `spawn` treats a failure
    /// after the fork as a state the run cannot be honest about and exits on it, on the stated
    /// ground that the answer was settled beforehand -- so an understating probe turns a host it
    /// misjudged into a guest that cannot start at all. Nightly cross-CPU replay reported exactly
    /// that shape: BOHR-1001, the guest could not be placed in its own namespaces.
    ///
    /// What this test does not do: fail on a host that grants the whole sequence, which is every
    /// host the fix could be tried on here -- an ordinary one and a process already inside an
    /// unmapped user namespace both granted all of it. It is capability-gated in the sense of
    /// docs/TESTING.md 9, and it discriminates only where the two answers can differ. The claim
    /// it pins is not that namespaces work, but that what is promised is what is performed.
    #[test]
    fn the_namespace_probe_promises_only_what_a_launch_performs() {
        let promised = namespaces_available();
        // SAFETY: between `fork` and `_exit` in a child that performs nothing else.
        let performed = probe_in_child(|| unsafe { spawn::enter_namespaces() });
        assert_eq!(
            promised, performed,
            "the probe and the launch disagree about this host, so one of them is wrong"
        );
    }
}
