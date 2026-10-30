//! Probes for what this host can and cannot do.
//!
//! Every probe that touches the machine lives here, behind the same boundary the backend does:
//! `ptrace`, `seccomp` and `prctl` are vocabulary of one crate, and a diagnosis is the one thing
//! that must name them out loud. What the answers mean, and what to do about them, belongs to
//! whoever is asking — this module only measures. See `docs/PLATFORMS.md` §2.

use bohr_backend::code::entropy;

/// Whether a host capability is present.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Probe {
    /// The capability is there.
    Present,
    /// It is not, and the run would be affected.
    Absent,
    /// It could not be established from here.
    Unknown,
}

impl Probe {
    /// A probe from a boolean answer.
    #[must_use]
    pub const fn from(present: bool) -> Self {
        if present {
            Self::Present
        } else {
            Self::Absent
        }
    }
}

/// Whether this process may trace another.
///
/// Probed by tracing a child rather than by reading a sysctl: a value that says tracing is
/// permitted and a kernel that refuses it are different facts, and only the second one matters.
#[must_use]
pub fn can_trace() -> Probe {
    // SAFETY: fork is async-signal-safe, and the child below reaches `_exit` on every path.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Probe::Unknown;
    }
    if pid == 0 {
        // SAFETY: the child of a fork, single-threaded by definition, asking to be traced and
        // then stopping itself so the parent can see it stopped.
        unsafe {
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) < 0 {
                libc::_exit(1);
            }
            libc::raise(libc::SIGSTOP);
            libc::_exit(0);
        }
    }

    let mut status = 0;
    // SAFETY: `pid` is the child just forked and `status` is a local.
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
    let traced = waited == pid && libc::WIFSTOPPED(status);
    // SAFETY: the child is ours, stopped or not; killing it releases it either way.
    unsafe {
        libc::ptrace(libc::PTRACE_KILL, pid, 0, 0);
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, &mut status, 0);
    }
    Probe::from(traced)
}

/// Whether the timestamp counter can be made to fault.
#[must_use]
pub fn can_trap_rdtsc() -> Probe {
    // SAFETY: a child of a fork, asking for the setting and reporting whether it took.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Probe::Unknown;
    }
    if pid == 0 {
        // SAFETY: PR_SET_TSC affects only this process, which is about to end.
        unsafe {
            let ok = libc::prctl(libc::PR_SET_TSC, libc::PR_TSC_SIGSEGV, 0, 0, 0) == 0;
            libc::_exit(i32::from(!ok));
        }
    }
    Probe::from(child_succeeded(pid))
}

/// Whether processor identification can be made to fault.
///
/// A hardware capability before it is a kernel one: without `cpuid_fault` the processor cannot
/// trap the instruction at all, which is why this is read from what the processor reports.
#[must_use]
pub fn can_trap_cpuid() -> Probe {
    match std::fs::read_to_string("/proc/cpuinfo") {
        Ok(text) => Probe::from(text.contains("cpuid_fault")),
        Err(_) => Probe::Unknown,
    }
}

/// Whether address randomisation can be turned off.
#[must_use]
pub fn can_disable_aslr() -> Probe {
    // SAFETY: a child of a fork, asking for the setting and reporting whether it took.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Probe::Unknown;
    }
    if pid == 0 {
        // SAFETY: personality affects only this process, which is about to end.
        unsafe {
            let ok = libc::personality(libc::ADDR_NO_RANDOMIZE as libc::c_ulong) >= 0;
            libc::_exit(i32::from(!ok));
        }
    }
    Probe::from(child_succeeded(pid))
}

/// Whether the real processor carries every feature named in the two words handed in.
///
/// The words are the ones a guest will be told it has, in the order the instruction returns them:
/// `ecx` of leaf one and `ebx` of leaf seven. Asked of the silicon and not of anything modelling
/// it, because the question is what will happen when the guest reaches such an instruction — and
/// the model's answer is precisely what cannot settle that.
///
/// Advertising is a promise. A guest told of a feature executes it, on the real processor,
/// whatever the world said; so a host that cannot keep the promise is a host where a guest dies on
/// an instruction nothing in the run could have predicted.
#[must_use]
pub fn supports(leaf_one_ecx: u32, leaf_seven_ebx: u32) -> Probe {
    // SAFETY: the instruction reads and writes nothing outside the registers it returns, and this
    // build only reaches here on the architecture that has it.
    let (one, seven) =
        unsafe { (std::arch::x86_64::__cpuid(1), std::arch::x86_64::__cpuid_count(7, 0)) };
    Probe::from(
        one.ecx & leaf_one_ecx == leaf_one_ecx && seven.ebx & leaf_seven_ebx == leaf_seven_ebx,
    )
}

/// Whether this user may create the namespaces a guest is isolated in.
///
/// Performs what a launch performs, rather than a subset of it. Asking for two of the five
/// namespaces and stopping at `unshare` let this report a capability the guest then failed to
/// obtain, which is the one thing a diagnostic must never do: a report that is wrong in the
/// permissive direction is worse than no report, because it is acted on.
#[must_use]
pub fn can_use_namespaces() -> Probe {
    // SAFETY: a child of a fork, entering the guest's namespaces and reporting whether it may.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Probe::Unknown;
    }
    if pid == 0 {
        // SAFETY: between `fork` and `_exit` in a child that performs nothing else, which is the
        // state `enter_namespaces` requires.
        unsafe {
            let ok = super::spawn::enter_namespaces();
            libc::_exit(i32::from(!ok));
        }
    }
    Probe::from(child_succeeded(pid))
}

/// Whether a filter can be installed at all.
#[must_use]
pub fn can_install_seccomp() -> Probe {
    // SAFETY: a child of a fork; both calls affect only it, and it exits immediately.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Probe::Unknown;
    }
    if pid == 0 {
        // SAFETY: sealing privileges and asking the kernel about seccomp, in a process that is
        // about to end either way.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                libc::_exit(1);
            }
            // Asking for a mode the kernel does not have fails differently from asking a kernel
            // that has no seccomp at all; either way the answer is the same to us.
            let supported = libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) >= 0;
            libc::_exit(i32::from(!supported));
        }
    }
    Probe::from(child_succeeded(pid))
}

/// Whether one process may read another's memory directly.
///
/// Its absence is a slowdown rather than a failure: the fallback reads a word at a time.
#[must_use]
pub fn can_read_memory_directly() -> Probe {
    let probe = 0x1234_5678u64;
    let mut out = 0u64;
    let local = libc::iovec {
        iov_base: std::ptr::addr_of_mut!(out).cast(),
        iov_len: std::mem::size_of::<u64>(),
    };
    let remote = libc::iovec {
        iov_base: std::ptr::addr_of!(probe) as *mut libc::c_void,
        iov_len: std::mem::size_of::<u64>(),
    };
    // SAFETY: both addresses are this process's own and both lengths match the object read.
    let read = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    Probe::from(read == std::mem::size_of::<u64>() as isize && out == probe)
}

/// Whether hardware branch counting is available, which is what fine progress measurement needs.
#[must_use]
pub fn can_count_branches() -> Probe {
    match std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid") {
        Ok(text) => Probe::from(text.trim().parse::<i32>().is_ok_and(|v| v <= 2)),
        Err(_) => Probe::Unknown,
    }
}

/// Whether this host is a Windows subsystem, which cannot trap processor identification.
#[must_use]
pub fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let v = v.to_lowercase();
            v.contains("microsoft") || v.contains("wsl")
        })
        .unwrap_or(false)
}

/// The value of a kernel setting, as text.
#[must_use]
pub fn sysctl(name: &str) -> Option<String> {
    let path = format!("/proc/sys/{}", name.replace('.', "/"));
    std::fs::read_to_string(path).ok().map(|v| v.trim().to_owned())
}

/// Whether the syscall the guest would use to pin itself to one core is permitted.
#[must_use]
pub fn can_pin_to_one_core() -> Probe {
    // SAFETY: an all-zero processor set is the empty one, which is what this starts from.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: the set is a local this call initialises.
    unsafe { libc::CPU_SET(0, &mut set) };
    // SAFETY: applying to this process, with a set it owns; the affinity is restored below.
    let applied = unsafe {
        let mut previous: libc::cpu_set_t = std::mem::zeroed();
        let saved =
            libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut previous) == 0;
        let ok = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0;
        if saved {
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &previous);
        }
        ok
    };
    Probe::from(applied)
}

/// Runs a child to completion and reports whether it exited zero.
fn child_succeeded(pid: libc::pid_t) -> bool {
    let mut status = 0;
    // SAFETY: `pid` is the child just forked and `status` is a local.
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    waited == pid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

/// What a program needs before it can be run: its loader, and instructions we cannot trap.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TargetReport {
    /// The dynamic loader the program names, where it names one.
    pub interpreter: Option<String>,
    /// Offsets of `RDRAND` or `RDSEED` in executable pages.
    ///
    /// Each is a place where the guest could take a number from the processor that no supervisor
    /// below a hypervisor can intercept — the one honest hole in the determinism map.
    pub hardware_random: Vec<u64>,
    /// Whether the program references `io_uring`, which is not modelled.
    pub uses_io_uring: bool,
}

/// Reads what an ELF program will need.
///
/// A minimal reader rather than a library: this is the only ELF this tool parses, and a
/// dependency for it would be a dependency in every build.
#[must_use]
pub fn inspect_target(bytes: &[u8]) -> Option<TargetReport> {
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 {
        return None;
    }
    let word = |at: usize| -> u64 {
        bytes.get(at..at + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    };
    let half = |at: usize| -> u16 {
        bytes.get(at..at + 2).map_or(0, |b| u16::from_le_bytes(b.try_into().unwrap_or_default()))
    };

    let ph_off = word(0x20) as usize;
    let ph_size = half(0x36) as usize;
    let ph_count = half(0x38) as usize;

    let mut report = TargetReport::default();
    for i in 0..ph_count {
        let at = ph_off + i * ph_size;
        let kind = bytes
            .get(at..at + 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap_or_default()));
        // PT_INTERP: the loader this program will ask the kernel for. The only header this reads
        // now -- where the instructions are is a question for whoever knows what one looks like.
        if kind == 3 {
            let offset = word(at + 0x08) as usize;
            let file_size = word(at + 0x20) as usize;
            let end = (offset + file_size).min(bytes.len());
            let text = bytes.get(offset..end).unwrap_or_default();
            let text = &text[..text.iter().position(|b| *b == 0).unwrap_or(text.len())];
            report.interpreter = std::str::from_utf8(text).ok().map(str::to_owned);
        }
    }
    // Every place an instruction takes a number from the processor, including the ones nothing
    // could establish: unjudged is not absent, and only the second may go unsaid.
    report.hardware_random = entropy::sites(bytes).into_iter().map(|site| site.at as u64).collect();
    report.uses_io_uring = references_io_uring(bytes);
    Some(report)
}

/// Keeps the candidates that sit where an instruction begins.
///
/// The scan below finds a byte pair, and a byte pair is `rdrand` only where an instruction starts:
/// the same two bytes inside a displacement are part of a load. Which is which is settled by
/// walking each function from its symbol and measuring every step, so the answer is a report about
/// the program rather than about its byte histogram.
///
/// Whether a program references the asynchronous submission interface.
fn references_io_uring(bytes: &[u8]) -> bool {
    // By name rather than by syscall number: the numbers are immediates that appear by chance in
    // any large binary, and the symbol is what a program that really uses it carries.
    let needle = b"io_uring";
    bytes.windows(needle.len()).any(|w| w == needle)
}
