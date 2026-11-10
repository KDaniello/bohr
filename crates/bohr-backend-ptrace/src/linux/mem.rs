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

/// Copies several pieces of the guest into `out`, one after another, in one crossing.
///
/// A short copy is read again piece by piece, so the error names the piece that could not be
/// read, as [`read`] does.
pub(super) fn read_pieces(
    pid: libc::pid_t,
    pieces: &[(GuestAddr, usize)],
    out: &mut [u8],
) -> Result<(), BackendError> {
    if out.is_empty() {
        return Ok(());
    }
    let remote: Vec<libc::iovec> = pieces
        .iter()
        .filter(|(_, len)| *len > 0)
        .map(|(addr, len)| libc::iovec { iov_base: addr.0 as *mut libc::c_void, iov_len: *len })
        .collect();
    let local = libc::iovec { iov_base: out.as_mut_ptr().cast(), iov_len: out.len() };

    // SAFETY: as in `read`: `local` is a buffer we own for the whole call, as long as the remote
    // pieces together; the kernel checks every remote one.
    let n = unsafe {
        libc::process_vm_readv(pid, &local, 1, remote.as_ptr(), remote.len() as libc::c_ulong, 0)
    };
    if n >= 0 && n as usize == out.len() {
        return Ok(());
    }
    let mut at = 0;
    for (addr, len) in pieces {
        read(pid, addr.0, &mut out[at..at + len])?;
        at += len;
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
