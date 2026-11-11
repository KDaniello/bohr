//! bohr's vDSO and the clock page beside it: a clock a guest reads without a stop.
//!
//! At the stop after every `execve`, the guest is made to map one page of clock and the image of
//! `vdso/vdso.c` above it, at a fixed address, and `AT_SYSINFO_EHDR` names the image. Before each
//! resume the supervisor writes the node's clock into the page; the image's `clock_gettime`,
//! `gettimeofday` and `time` read it, and fall back to the system call where the page holds
//! nothing. A read returns the clock as of the last stop, which is a function of the run; nothing
//! here reads the host's clock.

use bohr_backend::BackendError;

/// The image `build.rs` made, empty where this target has none.
static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bohr-vdso.so"));

/// Where the clock page goes, the image one page above it: in the high application range that
/// AddressSanitizer, ThreadSanitizer and MemorySanitizer all leave to the program, far below the
/// stack and the mappings the kernel hands out with randomisation off, so the same on every host.
const PAGE_AT: u64 = 0x7e00_0000_0000;
const PAGE: u64 = 4096;

/// `PT_LOAD`.
const LOAD: u32 = 1;

/// One loadable segment of the image: where it goes, its bytes, and the size it occupies.
struct Segment {
    vaddr: u64,
    bytes: &'static [u8],
    memsz: u64,
}

/// The image's loadable segments, read from its program headers; none where it is not an ELF64
/// image this build can place.
fn segments() -> Vec<Segment> {
    let word = |at: usize, len: usize| -> Option<u64> {
        let bytes = IMAGE.get(at..at + len)?;
        Some(bytes.iter().rev().fold(0u64, |n, b| n << 8 | u64::from(*b)))
    };
    let parse = || -> Option<Vec<Segment>> {
        if IMAGE.get(..4)? != b"\x7fELF" || *IMAGE.get(4)? != 2 {
            return None;
        }
        let (phoff, phentsize, phnum) = (word(0x20, 8)?, word(0x36, 2)?, word(0x38, 2)?);
        let mut out = Vec::new();
        for i in 0..phnum {
            let at = usize::try_from(phoff + i * phentsize).ok()?;
            if word(at, 4)? != u64::from(LOAD) {
                continue;
            }
            let (offset, vaddr) = (word(at + 8, 8)?, word(at + 16, 8)?);
            let (filesz, memsz) = (word(at + 32, 8)?, word(at + 40, 8)?);
            let start = usize::try_from(offset).ok()?;
            let bytes = IMAGE.get(start..start + usize::try_from(filesz).ok()?)?;
            out.push(Segment { vaddr, bytes, memsz });
        }
        Some(out)
    };
    parse().unwrap_or_default()
}

/// Maps the clock page and the image into the guest stopped at its `execve`, and names the
/// image's address for `AT_SYSINFO_EHDR`; `None` where this build has no image or the address is
/// taken, and the guest then runs without a vDSO, as before.
pub(super) fn install(pid: libc::pid_t) -> Result<Option<u64>, BackendError> {
    let segments = segments();
    let Some(end) = segments.iter().map(|s| s.vaddr + s.memsz).max() else {
        return Ok(None);
    };
    let span = PAGE + end.div_ceil(PAGE) * PAGE;
    // PROT_READ | PROT_WRITE; MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE. Never writable
    // and executable at once, which a host's policy may refuse.
    let mapped = guest_syscall(pid, MMAP, [PAGE_AT, span, 3, 0x22 | 0x10_0000, u64::MAX, 0])?;
    if mapped != PAGE_AT {
        return Ok(None);
    }
    let image = PAGE_AT + PAGE;
    for segment in &segments {
        super::mem::write(pid, image + segment.vaddr, segment.bytes)?;
    }
    // PROT_READ | PROT_EXEC for the image; the clock page stays writable for its reads.
    if guest_syscall(pid, MPROTECT, [image, span - PAGE, 5, 0, 0, 0])? != 0 {
        return Ok(None);
    }
    Ok(Some(image))
}

/// The clock page of an image at `image`.
pub(super) const fn page_of(image: u64) -> u64 {
    image - PAGE
}

/// Writes the node's clock into the page at `page`: valid, monotonic and wall, no reads yet, and
/// the step a read costs, in nanoseconds.
pub(super) fn present(
    pid: libc::pid_t,
    page: u64,
    monotonic_ns: u64,
    wall_ns: u64,
    step_ns: u64,
) -> Result<(), BackendError> {
    let mut bytes = [0u8; 40];
    for (i, word) in [1, monotonic_ns, wall_ns, 0, step_ns].into_iter().enumerate() {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }
    super::mem::write(pid, page, &bytes)
}

/// How many reads the image counted in the page at `page` since it was written.
pub(super) fn reads(pid: libc::pid_t, page: u64) -> Result<u64, BackendError> {
    let mut bytes = [0u8; 8];
    super::mem::read(pid, page + 24, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/// `mmap` and `mprotect`.
const MMAP: u8 = 9;
const MPROTECT: u8 = 10;

/// Runs system call `nr` inside the guest stopped at its `execve`, and puts everything back.
///
/// The kernel writes `execve`'s return into `rax` as it leaves the call, after this stop, so the
/// number cannot be set in a register: eight bytes at the entry point, `mov $nr, %eax; syscall;
/// int3`, set it themselves, and the trap after them hands the guest back here. A seccomp stop on
/// the way is let through, so the host answers it.
fn guest_syscall(pid: libc::pid_t, nr: u8, args: [u64; 6]) -> Result<u64, BackendError> {
    let lost = |what: &str| BackendError::Lost(format!("mapping the clock page: {what}"));
    let saved = super::get_regs(pid)?;
    let entry = saved.rip;
    let mut word = [0u8; 8];
    super::mem::read(pid, entry, &mut word).map_err(|_| lost("the entry point is unreadable"))?;
    let original = u64::from_le_bytes(word);
    // mov $nr, %eax (b8 nr 00 00 00); syscall (0f 05); int3 (cc).
    let code = u64::from_le_bytes([0xb8, nr, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xcc]);
    // SAFETY: as above; POKETEXT writes one word, which is restored below.
    if unsafe { libc::ptrace(libc::PTRACE_POKETEXT, pid, entry, code) } < 0 {
        return Err(lost("the entry point is not writable"));
    }
    let mut regs = saved;
    (regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9) =
        (args[0], args[1], args[2], args[3], args[4], args[5]);
    super::set_regs(pid, &regs)?;
    let result = (|| {
        loop {
            // SAFETY: the task is stopped and traced by us.
            if unsafe { libc::ptrace(libc::PTRACE_CONT, pid, 0, 0) } < 0 {
                return Err(lost("resuming the guest"));
            }
            let mut status = 0;
            // SAFETY: `pid` is our tracee and `status` a local we own.
            if unsafe { libc::waitpid(pid, &mut status, libc::__WALL) } < 0 {
                return Err(lost("waiting for the guest"));
            }
            if super::is_event(status, libc::PTRACE_EVENT_SECCOMP) {
                continue;
            }
            if libc::WIFSTOPPED(status)
                && libc::WSTOPSIG(status) == libc::SIGTRAP
                && status >> 16 == 0
            {
                return Ok(super::get_regs(pid)?.rax);
            }
            return Err(lost(&format!("an unexpected stop, status {status:#x}")));
        }
    })();
    // SAFETY: as above; the word read before is put back.
    unsafe { libc::ptrace(libc::PTRACE_POKETEXT, pid, entry, original) };
    // The program starts as `execve` leaves it: `rax` holds the call's zero, and no call is
    // pending to restart.
    let mut back = saved;
    (back.rax, back.orig_rax) = (0, u64::MAX);
    super::set_regs(pid, &back)?;
    result
}
