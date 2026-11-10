//! Guest memory access.

use bohr_backend::BackendError;
use bohr_types::GuestAddr;

/// Copies `out.len()` bytes out of the guest.
pub(super) fn read(pid: libc::pid_t, addr: u64, out: &mut [u8]) -> Result<(), BackendError> {
    if out.is_empty() {
        return Ok(());
    }
    let local = libc::iovec { iov_base: out.as_mut_ptr().cast(), iov_len: out.len() };
    let remote = libc::iovec { iov_base: addr as *mut libc::c_void, iov_len: out.len() };

    // SAFETY: `local` describes a buffer we own for the whole call; `remote` is interpreted by the
    // kernel, which reports a bad address with an error.
    let n = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if n < 0 || n as usize != out.len() {
        return Err(BackendError::BadAddress { addr: GuestAddr(addr), len: out.len() });
    }
    Ok(())
}

/// Copies `data` into the guest.
pub(super) fn write(pid: libc::pid_t, addr: u64, data: &[u8]) -> Result<(), BackendError> {
    if data.is_empty() {
        return Ok(());
    }
    let local = libc::iovec {
        iov_base: data.as_ptr().cast::<libc::c_void>().cast_mut(),
        iov_len: data.len(),
    };
    let remote = libc::iovec { iov_base: addr as *mut libc::c_void, iov_len: data.len() };

    // SAFETY: as in `read`; the local buffer is ours and is only read from.
    let n = unsafe { libc::process_vm_writev(pid, &local, 1, &remote, 1, 0) };
    if n < 0 || n as usize != data.len() {
        return Err(BackendError::BadAddress { addr: GuestAddr(addr), len: data.len() });
    }
    Ok(())
}
