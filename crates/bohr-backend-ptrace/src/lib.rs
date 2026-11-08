//! Syscall-level interception on Linux x86-64.
//!
//! This is the one crate that talks to the host and the one crate permitted `unsafe`; every block
//! carries the invariant its caller must hold. On any other target it compiles to nothing, so the
//! layers above build and test unchanged wherever the toolchain runs.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use linux::{PtraceBackend, doctor, syscall_name, syscall_table, unmodelled_syscall_name};

/// Whether this build contains a working interceptor.
///
/// Callers use it to say so plainly rather than failing in a way that looks like a defect.
#[must_use]
pub const fn available() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}
