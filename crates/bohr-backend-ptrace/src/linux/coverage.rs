//! The memory the guest writes its edge counters into.
//!
//! An anonymous file, mapped by both sides and passed to the guest as a descriptor. The guest has
//! its own IPC and mount namespaces, where a System V identifier or a `/dev/shm` path does not
//! reach it; a descriptor survives both the namespaces and `execve`.
//!
//! The region holds a header followed by the counters. The supervisor writes the counter count
//! into the header; passing it through the environment would change the run's digest with the map
//! size (`docs/API.md` §5.1). The guest writes its own edge count there, which shows whether the
//! map is big enough.
//!
//! See `docs/API.md` §5.5.

/// Where the guest finds the map. High enough that nothing the loader opens lands on it.
pub(super) const COVERAGE_FD: libc::c_int = 200;

/// The environment variable naming that descriptor, read by `bohr-cov-rt`.
pub(super) const COVERAGE_FD_VAR: &str = "BOHR_COV_FD";

/// Bytes before the counters. Four little-endian `u64`s, the last reserved.
const HEADER_BYTES: usize = 32;

/// The value the guest checks before reading the header. A guest with a mismatched runtime falls
/// back to its own buffer. Shared with `sdk/c/bohr-cov-rt.c`.
const HEADER_MAGIC: u64 = 0x0042_4F48_5243_5601;

/// The shared counters, open for as long as the backend is.
pub(super) struct CoverageChannel {
    fd: libc::c_int,
    /// The supervisor's own mapping of the same pages the guest writes.
    base: *mut u8,
    /// Counters after the header.
    cells: usize,
}

impl CoverageChannel {
    /// Opens a channel of `cells` counters, or `None` where the kernel has no anonymous files.
    ///
    /// Guests still run on such a host; the search just has no coverage.
    pub(super) fn open(cells: usize) -> Option<Self> {
        let cells = cells.max(1);
        let bytes = HEADER_BYTES + cells;
        // SAFETY: a constant name and a flag; the call allocates a descriptor or fails.
        let fd = unsafe { libc::memfd_create(c"bohr-cov".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` is the descriptor just created and the length is our own.
        if unsafe { libc::ftruncate(fd, bytes as libc::off_t) } != 0 {
            // SAFETY: closing a descriptor this function owns and has not published.
            unsafe { libc::close(fd) };
            return None;
        }
        // SAFETY: mapping the file just sized, with a length that matches it.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            // SAFETY: as above.
            unsafe { libc::close(fd) };
            return None;
        }
        let channel = Self { fd, base: base.cast::<u8>(), cells };
        channel.put(0, HEADER_MAGIC);
        channel.put(8, cells as u64);
        Some(channel)
    }

    /// Writes one header word.
    fn put(&self, at: usize, value: u64) {
        // SAFETY: `at` is within the header, which is mapped writable for this channel's lifetime.
        unsafe {
            std::ptr::copy_nonoverlapping(value.to_le_bytes().as_ptr(), self.base.add(at), 8);
        }
    }

    /// The descriptor to place in the guest.
    pub(super) const fn fd(&self) -> libc::c_int {
        self.fd
    }

    /// How many edges the guest was instrumented with, as the guest reported.
    ///
    /// Zero from a guest without the coverage runtime. The caller tells that apart from a guest
    /// with no edges by the map being empty.
    pub(super) fn guards(&self) -> u64 {
        let mut word = [0u8; 8];
        // SAFETY: the word is within the header, mapped readable for this channel's lifetime.
        unsafe { std::ptr::copy_nonoverlapping(self.base.add(16), word.as_mut_ptr(), 8) };
        u64::from_le_bytes(word)
    }

    /// Copies the counters out.
    ///
    /// Called after the run loop ends, so the search never depends on when it looked (§5.1).
    ///
    /// A guest killed at the end may not be reaped yet and can still increment a counter, which
    /// then reads one increment stale. Bucketing absorbs that, and coverage is outside the trace
    /// digest (§5.10).
    pub(super) fn read_into(&self, out: &mut [u8]) {
        let len = out.len().min(self.cells);
        // SAFETY: the counters begin HEADER_BYTES into a mapping of HEADER_BYTES + cells readable
        // bytes that lives as long as this channel, and `len` never exceeds `cells`. A concurrent
        // increment can make a byte stale but cannot make the read invalid: the mapping stays
        // mapped, and a byte is read atomically on this architecture.
        unsafe {
            std::ptr::copy_nonoverlapping(self.base.add(HEADER_BYTES), out.as_mut_ptr(), len);
        }
    }
}

impl Drop for CoverageChannel {
    fn drop(&mut self) {
        // SAFETY: both were obtained by this channel and are released exactly once.
        unsafe {
            libc::munmap(self.base.cast::<libc::c_void>(), HEADER_BYTES + self.cells);
            libc::close(self.fd);
        }
    }
}

impl std::fmt::Debug for CoverageChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverageChannel")
            .field("fd", &self.fd)
            .field("cells", &self.cells)
            .finish_non_exhaustive()
    }
}
