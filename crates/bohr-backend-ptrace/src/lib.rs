//! Syscall-level interception on Linux x86-64.
//!
//! The only crate that talks to the host and the only one allowed `unsafe`; every unsafe block
//! states its invariant. On other targets it compiles to nothing, so the layers above build and
//! test anywhere.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use linux::{
    CORE_VARIABLE, PtraceBackend, doctor, syscall_name, syscall_table, unmodelled_syscall_name,
};

/// Whether this build contains a working interceptor.
///
/// Callers check it to report a missing interceptor clearly.
#[must_use]
pub const fn available() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}
