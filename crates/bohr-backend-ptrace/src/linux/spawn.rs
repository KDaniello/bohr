//! Starting a guest with every host-derived variable already removed.
//!
//! Everything here happens between `fork` and `execve`, or at the exec stop immediately after, so
//! the guest never executes an instruction in a state we did not choose.

use std::{
    ffi::{CString, OsStr},
    os::unix::ffi::OsStrExt,
};

use bohr_backend::{BackendError, GuestSpec};
use bohr_types::GuestAddr;

use super::{coverage, syscalls};

/// Auxiliary vector entry naming the VDSO. Zeroing it leaves the loader nothing to map, so calls
/// that would have been answered in userspace become real syscalls and reach the filter.
const AT_SYSINFO_EHDR: u64 = 33;

/// Auxiliary vector entry carrying sixteen random bytes. glibc derives stack canaries and hash
/// seeds from them, so leaving them alone would randomise the guest's own iteration orders.
const AT_RANDOM: u64 = 25;

/// Terminator of the auxiliary vector.
const AT_NULL: u64 = 0;

/// Namespaces the guest is placed in, so that what it can observe of the machine is ours to define
/// rather than the host's to leak.
///
/// The identifier namespace is what makes thread numbering reproducible: glibc stores a thread's
/// own identifier in memory and waits on it, so a guest that inherited host numbering would differ
/// from itself on the second run.
///
/// **There is no network namespace here, and the omission is deliberate.** What confines the guest
/// to the modelled network is the filter, not an empty interface list: the filter's default action
/// is to trace, the admitted list holds only calls that shape the guest's own address space or
/// signal mask, and so every network call either reaches the model or is refused by name. A guest
/// cannot open a socket the host would honour whether or not it has interfaces to try.
///
/// The namespace was therefore protecting against nothing that can occur, and it is not free: it
/// costs about 180 KB of kernel memory that cannot be reclaimed against 4 KB for all the others
/// together, the kernel releases it on a lagging schedule, and a campaign starts hundreds of
/// guests. Consecutive campaigns ran their host out of it and could no longer place a guest at all.
pub(super) const GUEST_NAMESPACES: libc::c_int = libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNS
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC;

/// Machine name the guest sees, in place of whichever host it happens to be on.
const GUEST_HOSTNAME: &[u8] = b"bohr";

/// Exit codes the child uses when a step of its own setup fails.
///
/// Between `fork` and `execve` there is nothing to report through — no allocation, no channel, and
/// a signal would be indistinguishable from the guest crashing. The exit code is the only channel,
/// and each step gets its own so a failure names the step rather than a wait status.
mod failed {
    pub(super) const PERSONALITY: i32 = 101;
    pub(super) const AFFINITY: i32 = 102;
    pub(super) const TRACEME: i32 = 103;
    pub(super) const NO_NEW_PRIVS: i32 = 104;
    pub(super) const SECCOMP: i32 = 105;
    pub(super) const TSC: i32 = 106;
    pub(super) const EXECVE: i32 = 107;
    pub(super) const NAMESPACES: i32 = 108;
    pub(super) const HANDOVER: i32 = 109;
    pub(super) const COVERAGE: i32 = 110;
}

/// What a child exit code means, where it means anything.
pub(super) fn setup_failure(code: i32) -> Option<&'static str> {
    Some(match code {
        failed::PERSONALITY => "address randomisation could not be disabled",
        failed::AFFINITY => "the guest could not be pinned to one core",
        failed::TRACEME => "the guest could not be put under tracing",
        failed::NO_NEW_PRIVS => "privilege escalation could not be sealed off",
        failed::SECCOMP => "the syscall filter could not be installed",
        failed::TSC => "the timestamp counter could not be made to fault",
        failed::EXECVE => "the program could not be executed",
        failed::NAMESPACES => "the guest could not be placed in its own namespaces",
        failed::HANDOVER => "the guest could not be handed over to the supervisor",
        failed::COVERAGE => "the coverage counters could not be placed in the guest",
        _ => return None,
    })
}

/// Starts the guest and returns the pid the supervisor traces.
///
/// A guest that is to see its own process numbering must be *inside* a new identifier namespace,
/// and a process only ever enters one by being created after it exists. So the guest is a
/// grandchild: the intermediate creates the namespace and the guest in it, then leaves. The
/// supervisor attaches to the grandchild rather than being handed it by `PTRACE_TRACEME`, since
/// that names the immediate parent, which is the intermediate and not us.
pub(super) fn spawn_traced(
    spec: &GuestSpec,
    isolate: bool,
    coverage: Option<libc::c_int>,
) -> Result<libc::pid_t, BackendError> {
    let program = spec.argv.first().ok_or_else(|| BackendError::CannotStart {
        program: "<none>".to_owned(),
        reason: "no program given".to_owned(),
    })?;

    let argv = to_c_strings(&spec.argv)?;
    let mut envp: Vec<CString> = spec
        .env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")).unwrap_or_default())
        .collect();
    // Where the guest's coverage runtime will find its counters. Added here rather than by the
    // caller because the number is this module's to choose: it is the one the descriptor is moved
    // to below, and the two must not be able to disagree.
    // How large the map is is *not* here, and that is the point: the guest reads it out of the
    // region's own header. In the environment it would be part of what the guest was started with,
    // and one run would digest differently from the same run searched with a different map.
    if coverage.is_some() {
        let named = format!("{}={}", coverage::COVERAGE_FD_VAR, coverage::COVERAGE_FD);
        envp.push(CString::new(named).unwrap_or_default());
    }
    let path = CString::new(program.as_bytes()).map_err(|_| BackendError::CannotStart {
        program: program.clone(),
        reason: "program path contains a NUL".to_owned(),
    })?;

    let filter = syscalls::filter_program().map_err(|e| BackendError::CannotStart {
        program: program.clone(),
        reason: format!("cannot build the syscall filter: {e}"),
    })?;

    // Two pipes: one carries the guest's pid outwards, the other tells the guest it is being
    // traced. Without the second, the guest could reach its filter before the supervisor is
    // watching, and a filter that traces with no tracer attached fails the syscall instead.
    let mut report = [0; 2];
    let mut go = [0; 2];
    // Close-on-exec, so the ends the guest does not use do not follow it through `execve`. They
    // used to: a guest reading `/proc/self/fd` saw two descriptors whose numbers came from this
    // process's allocation state, which is host state reaching a guest by the back door. The
    // coverage channel two files away is handled exactly this way for exactly this reason, and
    // the ends the guest is meant to inherit clear the flag where they are dup'd into place.
    // SAFETY: both arrays are ours and large enough for a pipe pair.
    if unsafe { libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(BackendError::CannotStart { program: program.clone(), reason: last_error() });
    }
    // SAFETY: as above; the first pair is closed on every failing path out of here.
    if unsafe { libc::pipe2(go.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        let reason = last_error();
        // SAFETY: descriptors this call opened and nothing else holds.
        unsafe {
            libc::close(report[0]);
            libc::close(report[1]);
        }
        return Err(BackendError::CannotStart { program: program.clone(), reason });
    }

    // SAFETY: fork is async-signal-safe. Neither child returns to the caller: every path ends in
    // execve or _exit.
    let intermediate = unsafe { libc::fork() };
    if intermediate < 0 {
        let reason = last_error();
        // A campaign that starts hundreds of guests and meets a descriptor ceiling used to leak
        // four here on every attempt after it, so the ceiling became permanent.
        // SAFETY: descriptors this call opened and nothing else holds.
        unsafe {
            libc::close(report[0]);
            libc::close(report[1]);
            libc::close(go[0]);
            libc::close(go[1]);
        }
        return Err(BackendError::CannotStart { program: program.clone(), reason });
    }
    if intermediate == 0 {
        // SAFETY: the child of a fork, single-threaded by definition, calling only
        // async-signal-safe functions.
        unsafe {
            intermediate_setup(&path, &argv, &envp, &filter, isolate, report[1], go[0], coverage)
        }
    }

    // SAFETY: descriptors this process owns; the ends the guest uses are closed here so a read
    // blocks until the writer is gone rather than forever.
    unsafe {
        libc::close(report[1]);
        libc::close(go[0]);
    }

    let mut buf = [0u8; 4];
    // SAFETY: `buf` is ours and the descriptor is open.
    let read = unsafe { libc::read(report[0], buf.as_mut_ptr().cast(), buf.len()) };
    // SAFETY: descriptors this process owns.
    unsafe { libc::close(report[0]) };
    let mut status = 0;
    // SAFETY: the intermediate is our child; reaping it leaves no zombie behind.
    unsafe { libc::waitpid(intermediate, &mut status, 0) };

    if read != buf.len() as isize {
        // SAFETY: our own descriptor.
        unsafe { libc::close(go[1]) };
        return Err(BackendError::CannotStart {
            program: program.clone(),
            reason: describe_setup_failure(status),
        });
    }
    let guest = libc::pid_t::from_le_bytes(buf);

    let options = trace_options();
    // SAFETY: `guest` is our own descendant, which is what makes attaching to it permitted.
    let seized = unsafe { libc::ptrace(libc::PTRACE_SEIZE, guest, 0, options) };
    // Releasing the guest is what lets it install its filter, and it must not happen before the
    // line above: a filter that traces with no tracer attached turns every syscall into ENOSYS.
    // SAFETY: our own descriptor, written once and then closed.
    unsafe {
        libc::write(go[1], b"g".as_ptr().cast(), 1);
        libc::close(go[1]);
    }
    if seized < 0 {
        return Err(BackendError::CannotStart {
            program: program.clone(),
            reason: format!("attaching to the guest: {}", last_error()),
        });
    }
    Ok(guest)
}

/// Trace options every task carries, named here because the guest is attached to before it runs.
pub(super) const fn trace_options() -> libc::c_int {
    libc::PTRACE_O_TRACESECCOMP
        | libc::PTRACE_O_TRACESYSGOOD
        | libc::PTRACE_O_TRACEEXEC
        | libc::PTRACE_O_TRACECLONE
        | libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_EXITKILL
}

/// What the intermediate's exit status says about why no guest arrived.
fn describe_setup_failure(status: libc::c_int) -> String {
    if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        if let Some(step) = setup_failure(code) {
            return step.to_owned();
        }
    }
    "the guest was not started".to_owned()
}

/// Creates the namespaces, then the guest inside them, and reports the guest outwards.
///
/// # Safety
///
/// Must only be called in the child of a `fork`. It never returns.
unsafe fn intermediate_setup(
    path: &CString,
    argv: &[CString],
    envp: &[CString],
    filter: &[seccompiler::sock_filter],
    isolate: bool,
    report: libc::c_int,
    go: libc::c_int,
    coverage: Option<libc::c_int>,
) -> ! {
    // Whether this host allows it is settled before the fork, so reaching here and failing is a
    // condition the run cannot be honest about and must not continue under.
    if isolate && !enter_namespaces() {
        libc::_exit(failed::NAMESPACES);
    }

    // The guest, and only the guest, is inside the identifier namespace created above.
    let guest = libc::fork();
    if guest < 0 {
        libc::_exit(failed::HANDOVER);
    }
    if guest == 0 {
        libc::close(report);
        child_setup(path, argv, envp, filter, go, coverage)
    }

    // The pid here is the one the supervisor can see, which is the outer numbering; inside its own
    // namespace the guest is number one.
    let bytes = guest.to_le_bytes();
    if libc::write(report, bytes.as_ptr().cast(), bytes.len()) != bytes.len() as isize {
        libc::_exit(failed::HANDOVER);
    }
    libc::_exit(0)
}

/// Everything the guest must have arranged before its first instruction.
///
/// # Safety
///
/// Must only be called in the child of a `fork`, where the process is single-threaded and the
/// address space is still shared with nothing.
unsafe fn child_setup(
    path: &CString,
    argv: &[CString],
    envp: &[CString],
    filter: &[seccompiler::sock_filter],
    go: libc::c_int,
    coverage: Option<libc::c_int>,
) -> ! {
    // The counters, moved to the number the environment names. `dup2` is what carries them across
    // the `execve`: it clears close-on-exec on the descriptor it creates, and the original was
    // opened with it set so that no other spawn inherits one.
    if let Some(fd) = coverage {
        if libc::dup2(fd, coverage::COVERAGE_FD) < 0 {
            libc::_exit(failed::COVERAGE);
        }
    }
    // Address randomisation would place every mapping differently on every run.
    if libc::personality(libc::ADDR_NO_RANDOMIZE as libc::c_ulong) == -1 {
        libc::_exit(failed::PERSONALITY);
    }

    // One core: migration between cores changes timing and what the processor reports.
    let mut set: libc::cpu_set_t = std::mem::zeroed();
    libc::CPU_SET(0, &mut set);
    if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == -1 {
        libc::_exit(failed::AFFINITY);
    }

    // A filter may only be installed by a process that cannot gain privileges, and the guarantee
    // must outlive the exec.
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
        libc::_exit(failed::NO_NEW_PRIVS);
    }
    // Reading the timestamp counter becomes a fault the supervisor answers from logical time.
    // Set before the filter, because `prctl` is itself filtered and would otherwise be refused.
    if libc::prctl(libc::PR_SET_TSC, libc::PR_TSC_SIGSEGV, 0, 0, 0) == -1 {
        libc::_exit(failed::TSC);
    }

    // Wait until the supervisor is watching. The order is load-bearing: a filter returning
    // SECCOMP_RET_TRACE while no tracer is attached produces no event at all and fails the syscall
    // with ENOSYS instead, which surfaces much later as a segfault in the loader.
    let mut ready = [0u8; 1];
    if libc::read(go, ready.as_mut_ptr().cast(), 1) != 1 {
        libc::_exit(failed::HANDOVER);
    }
    libc::close(go);

    // Both crates describe the kernel's `struct sock_filter`, so the cast is between two spellings
    // of one ABI type.
    let prog = libc::sock_fprog {
        len: u16::try_from(filter.len()).unwrap_or(u16::MAX),
        filter: filter.as_ptr().cast::<libc::sock_filter>().cast_mut(),
    };
    if libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, 0, std::ptr::from_ref(&prog))
        == -1
    {
        libc::_exit(failed::SECCOMP);
    }

    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    libc::execve(path.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    libc::_exit(failed::EXECVE)
}

/// Places the caller in fresh namespaces and gives it a fixed view of the machine.
///
/// # Safety
///
/// Must only be called between `fork` and `execve`, where the process is single-threaded.
pub(super) unsafe fn enter_namespaces() -> bool {
    let uid = libc::getuid();
    let gid = libc::getgid();
    if libc::unshare(GUEST_NAMESPACES) == -1 {
        return false;
    }

    // A fresh user namespace starts with no mapping, which leaves every identifier unresolvable
    // until one is written. Group mapping is refused outright unless `setgroups` is denied first.
    let mut line = [0u8; 32];
    if !write_proc(c"/proc/self/setgroups", b"deny")
        || !write_proc(c"/proc/self/uid_map", identity_map(&mut line, uid))
        || !write_proc(c"/proc/self/gid_map", identity_map(&mut line, gid))
    {
        return false;
    }

    libc::sethostname(GUEST_HOSTNAME.as_ptr().cast(), GUEST_HOSTNAME.len()) == 0
        && libc::setdomainname(GUEST_HOSTNAME.as_ptr().cast(), GUEST_HOSTNAME.len()) == 0
}

/// Formats `0 <id> 1`, the one mapping line an unprivileged process is allowed to install.
fn identity_map(buf: &mut [u8; 32], id: libc::uid_t) -> &[u8] {
    let mut digits = [0u8; 10];
    let mut n = id;
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }

    buf[..2].copy_from_slice(b"0 ");
    let end = 2 + (digits.len() - i);
    buf[2..end].copy_from_slice(&digits[i..]);
    buf[end..end + 2].copy_from_slice(b" 1");
    &buf[..end + 2]
}

/// Writes one line to a `procfs` control file, which accepts a whole line or nothing.
///
/// # Safety
///
/// Must only be called between `fork` and `execve`.
unsafe fn write_proc(path: &std::ffi::CStr, line: &[u8]) -> bool {
    let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return false;
    }
    let written = libc::write(fd, line.as_ptr().cast(), line.len());
    libc::close(fd);
    written == line.len() as isize
}

/// Rewrites the auxiliary vector on the guest's stack.
///
/// Called once, at the stop that follows `execve`, when the stack pointer still points at
/// `argc`. Walks past the arguments and the environment to reach the vector, then neutralises the
/// two entries that would otherwise carry host state into the guest.
pub(super) fn neutralise_auxv(pid: libc::pid_t, rsp: u64) -> Result<AuxvReport, BackendError> {
    let mut addr = rsp;
    let argc = read_word(pid, addr)?;
    // Checked, because `argc` comes out of the guest's memory and everything after it is address
    // arithmetic. The kernel builds this stack and bounds `argc` by `ARG_MAX`, so nothing here is
    // expected to overflow -- but "expected" is the word that makes a release build wrap silently
    // where a debug build would have panicked, and this walk runs again at every later `execve`.
    let bad = || BackendError::BadAddress { addr: GuestAddr(rsp), len: 8 };
    addr = argc
        .checked_add(1)
        .and_then(|slots| slots.checked_mul(8))
        .and_then(|argv| addr.checked_add(8)?.checked_add(argv))
        .ok_or_else(bad)?; // argv, including its NULL

    // envp, including its NULL.
    loop {
        let word = read_word(pid, addr)?;
        addr = addr.checked_add(8).ok_or_else(bad)?;
        if word == 0 {
            break;
        }
    }

    let mut report = AuxvReport::default();
    loop {
        let key = read_word(pid, addr)?;
        if key == AT_NULL {
            break;
        }
        let value_at = addr + 8;
        match key {
            AT_SYSINFO_EHDR => {
                write_word(pid, value_at, 0)?;
                report.vdso_disabled = true;
            }
            AT_RANDOM => {
                let buf = read_word(pid, value_at)?;
                write_bytes(pid, buf, &[0u8; 16])?;
                report.at_random_zeroed = true;
            }
            _ => {}
        }
        addr += 16;
    }
    Ok(report)
}

/// What the auxiliary vector rewrite actually managed to do.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct AuxvReport {
    /// Whether a VDSO entry was present and cleared.
    pub vdso_disabled: bool,
    /// Whether the random bytes were present and zeroed.
    pub at_random_zeroed: bool,
}

fn read_word(pid: libc::pid_t, addr: u64) -> Result<u64, BackendError> {
    let mut buf = [0u8; 8];
    super::mem::read(pid, addr, &mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn write_word(pid: libc::pid_t, addr: u64, value: u64) -> Result<(), BackendError> {
    write_bytes(pid, addr, &value.to_le_bytes())
}

fn write_bytes(pid: libc::pid_t, addr: u64, data: &[u8]) -> Result<(), BackendError> {
    super::mem::write(pid, addr, data)
}

fn to_c_strings(items: &[String]) -> Result<Vec<CString>, BackendError> {
    items
        .iter()
        .map(|s| {
            CString::new(OsStr::new(s).as_bytes()).map_err(|_| BackendError::CannotStart {
                program: s.clone(),
                reason: "argument contains a NUL".to_owned(),
            })
        })
        .collect()
}

pub(super) fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}
