//! Linux x86-64 numbering shared by every model in this crate: syscalls and errnos.
//!
//! Spelled out here so no model needs libc, whose constants belong to the host rather than to
//! the world the guest is shown.

/// Linux x86-64 syscall numbers this model answers.
pub(crate) mod sys {
    /// `read`.
    pub(crate) const READ: u32 = 0;
    /// `write`.
    pub(crate) const WRITE: u32 = 1;
    /// `writev`.
    pub(crate) const WRITEV: u32 = 20;
    /// `pwritev`.
    pub(crate) const PWRITEV: u32 = 296;
    /// `close`.
    pub(crate) const CLOSE: u32 = 3;
    /// `getpid`.
    pub(crate) const GETPID: u32 = 39;
    /// `futex`.
    pub(crate) const FUTEX: u32 = 202;
    /// `clone`.
    pub(crate) const CLONE: u32 = 56;
    /// `clone3`.
    pub(crate) const CLONE3: u32 = 435;
    /// `lseek`.
    pub(crate) const LSEEK: u32 = 8;
    /// `pread64`.
    pub(crate) const PREAD64: u32 = 17;
    /// `pwrite64`.
    pub(crate) const PWRITE64: u32 = 18;
    /// `fstat`.
    pub(crate) const FSTAT: u32 = 5;
    /// `ftruncate`.
    pub(crate) const FTRUNCATE: u32 = 77;
    /// `fsync`.
    pub(crate) const FSYNC: u32 = 74;
    /// `fdatasync`.
    pub(crate) const FDATASYNC: u32 = 75;
    /// `flock`.
    pub(crate) const FLOCK: u32 = 73;
    /// `unlink`.
    pub(crate) const UNLINK: u32 = 87;
    /// `unlinkat`.
    pub(crate) const UNLINKAT: u32 = 263;
    /// `rename`.
    pub(crate) const RENAME: u32 = 82;
    /// `renameat2`.
    pub(crate) const RENAMEAT2: u32 = 316;
    /// `statx`.
    pub(crate) const STATX: u32 = 332;
    /// `newfstatat`.
    pub(crate) const NEWFSTATAT: u32 = 262;
    /// `open`: `openat` without the descriptor to be relative to, which nothing here is.
    pub(crate) const OPEN: u32 = 2;
    /// `getrusage`.
    pub(crate) const GETRUSAGE: u32 = 98;
    /// `getrlimit`.
    pub(crate) const GETRLIMIT: u32 = 97;
    /// `setrlimit`.
    pub(crate) const SETRLIMIT: u32 = 160;
    /// `prlimit64`, which is `getrlimit` and `setrlimit` in one call.
    pub(crate) const PRLIMIT64: u32 = 302;
    /// `statfs`.
    pub(crate) const STATFS: u32 = 137;
    /// `fstatfs`.
    pub(crate) const FSTATFS: u32 = 138;
    /// `gettid`.
    pub(crate) const GETTID: u32 = 186;
    /// `sched_getaffinity`.
    pub(crate) const SCHED_GETAFFINITY: u32 = 204;
    /// `sched_yield`.
    pub(crate) const SCHED_YIELD: u32 = 24;
    /// `prctl`.
    pub(crate) const PRCTL: u32 = 157;
    /// `poll`.
    pub(crate) const POLL: u32 = 7;
    /// `fadvise64`.
    pub(crate) const FADVISE64: u32 = 221;
    /// `nanosleep`.
    pub(crate) const NANOSLEEP: u32 = 35;
    /// `clock_nanosleep`.
    pub(crate) const CLOCK_NANOSLEEP: u32 = 230;
    /// `ioctl`.
    pub(crate) const IOCTL: u32 = 16;
    /// `access`.
    pub(crate) const ACCESS: u32 = 21;
    /// `faccessat2`.
    pub(crate) const FACCESSAT2: u32 = 439;
    /// `fcntl`.
    pub(crate) const FCNTL: u32 = 72;
    /// `getcwd`.
    pub(crate) const GETCWD: u32 = 79;
    /// `uname`.
    pub(crate) const UNAME: u32 = 63;
    /// `getuid`.
    pub(crate) const GETUID: u32 = 102;
    /// `getgid`.
    pub(crate) const GETGID: u32 = 104;
    /// `geteuid`.
    pub(crate) const GETEUID: u32 = 107;
    /// `getegid`.
    pub(crate) const GETEGID: u32 = 108;
    /// `tgkill`.
    pub(crate) const TGKILL: u32 = 234;
    /// `kill`.
    pub(crate) const KILL: u32 = 62;
    /// `getdents64`.
    pub(crate) const GETDENTS64: u32 = 217;
    /// `sysinfo`.
    pub(crate) const SYSINFO: u32 = 99;
    /// `mkdir`.
    pub(crate) const MKDIR: u32 = 83;
    /// `mkdirat`.
    pub(crate) const MKDIRAT: u32 = 258;
    /// `readlink`.
    pub(crate) const RENAMEAT: u32 = 264;
    pub(crate) const RMDIR: u32 = 84;
    pub(crate) const TIME: u32 = 201;
    pub(crate) const DUP: u32 = 32;
    pub(crate) const DUP2: u32 = 33;
    pub(crate) const DUP3: u32 = 292;
    pub(crate) const SYMLINK: u32 = 88;
    pub(crate) const READLINK: u32 = 89;
    /// `pipe2`.
    pub(crate) const PIPE2: u32 = 293;
    /// `getpgrp`.
    pub(crate) const GETPGRP: u32 = 111;
    /// `getppid`.
    pub(crate) const GETPPID: u32 = 110;
    /// `sched_getparam`.
    pub(crate) const SCHED_GETPARAM: u32 = 143;
    /// `sched_getscheduler`.
    pub(crate) const SCHED_GETSCHEDULER: u32 = 145;
    /// `gettimeofday`.
    pub(crate) const GETTIMEOFDAY: u32 = 96;
    /// `exit`.
    pub(crate) const EXIT: u32 = 60;
    /// `clock_gettime`.
    pub(crate) const CLOCK_GETTIME: u32 = 228;
    /// `exit_group`.
    pub(crate) const EXIT_GROUP: u32 = 231;
    /// `openat`.
    pub(crate) const OPENAT: u32 = 257;
    /// `getrandom`.
    pub(crate) const GETRANDOM: u32 = 318;
    /// `readlinkat`.
    pub(crate) const SYMLINKAT: u32 = 266;
    pub(crate) const READLINKAT: u32 = 267;

    /// `io_uring_setup`, `io_uring_enter`, `io_uring_register`.
    ///
    /// Named in order to be refused by name. They are not in the interceptor's table and never
    /// will be while the ring is out of scope — the table drives the filter, and these reach the
    /// supervisor through its default action like anything else it does not list.
    /// Opening a ring, which is declined rather than refused: see `MinimalWorld::decline_ring`.
    pub(crate) const IO_URING_SETUP: u32 = 425;
    pub(crate) const IO_URING: [u32; 3] = [425, 426, 427];

    /// `mmap`, which reaches here only where it named a file: the filter admits an anonymous
    /// mapping, which shapes the guest's own address space and nothing this model holds.
    pub(crate) const MMAP: u32 = 9;
    /// `munmap`.
    pub(crate) const MUNMAP: u32 = 11;
    /// `msync`.
    pub(crate) const MSYNC: u32 = 26;
}

/// Errors returned to the guest, spelled out so this crate needs no libc.
pub(crate) mod errno {
    /// No such file or directory.
    pub(crate) const ENOENT: i64 = 2;
    /// Bad file descriptor.
    pub(crate) const EBADF: i64 = 9;
    /// Try again: the value a wait was predicated on had already changed.
    pub(crate) const EAGAIN: i64 = 11;
    /// Not permitted for this operation.
    pub(crate) const EINVAL: i64 = 22;
    /// File name too long.
    pub(crate) const ENAMETOOLONG: i64 = 36;
    /// The operation is not supported by this object.
    pub(crate) const ENOSYS: i64 = 38;
    /// Not a terminal.
    pub(crate) const ENOTTY: i64 = 25;
    /// The answer does not fit in the buffer offered.
    pub(crate) const ERANGE: i64 = 34;
    /// The device failed mid-operation.
    pub(crate) const EIO: i64 = 5;
    /// The disk is full.
    pub(crate) const ENOSPC: i64 = 28;
    /// The file would grow past the largest one this model holds.
    pub(crate) const EFBIG: i64 = 27;
    /// The address named is not one the guest could have.
    pub(crate) const EFAULT: i64 = 14;
    /// A timed wait ran out before anything released it.
    pub(crate) const ETIMEDOUT: i64 = 110;
    /// The name is already taken.
    pub(crate) const EEXIST: i64 = 17;
    /// The name is a directory, and the operation is one a directory refuses.
    pub(crate) const EISDIR: i64 = 21;
    /// The name is not a directory, and the caller required one.
    pub(crate) const ENOTDIR: i64 = 20;
    /// Written to a stream whose other end is gone.
    pub(crate) const EPIPE: i64 = 32;
    /// The socket is already connected.
    pub(crate) const EISCONN: i64 = 106;
    /// The socket is not connected, and the operation needs it to be.
    pub(crate) const ENOTCONN: i64 = 107;
    /// Nobody is listening on the address named.
    pub(crate) const ECONNREFUSED: i64 = 111;
    /// A connection was started and has not finished; the caller asked not to wait.
    pub(crate) const EINPROGRESS: i64 = 115;

    /// The address a socket asked to bind is already in use.
    pub(crate) const EADDRINUSE: i64 = 98;
    /// A value is too large for the type that would carry it back.
    pub(crate) const EOVERFLOW: i64 = 75;
    /// The operation asked for would deadlock the caller against itself.
    pub(crate) const EDEADLK: i64 = 35;
    /// A directory still holds entries.
    pub(crate) const ENOTEMPTY: i64 = 39;
}
