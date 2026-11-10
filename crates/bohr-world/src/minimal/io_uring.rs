//! IoUring: the io_uring calls, declined or carried out.

use super::*;

/// The calls this subsystem answers.
pub(super) const HANDLES: &[u32] =
    &[sys::IO_URING_SETUP, sys::IO_URING_REGISTER, sys::IO_URING_ENTER];

impl MinimalWorld {
    /// Answers one of [`HANDLES`].
    pub(super) fn answer_io_uring(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ctx: &mut StopCtx<'_, '_>,
    ) -> Result<Answer, KernelError> {
        Ok(Answer::Reply(match nr {
            // A ring under `[linux] io_uring = "ring"`, otherwise `ENOSYS`.
            sys::IO_URING_SETUP if self.io_uring => {
                let ret = self.open_ring(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::IO_URING_SETUP => {
                let ret = self.decline_ring(ctx)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Registration is refused; callers fall back, as on kernels limiting locked memory.
            sys::IO_URING_REGISTER if self.io_uring => {
                let ret = -errno::EINVAL;
                (ret, Resume::Emulate { ret }, None)
            }
            // Rings are entered through the cluster; here the descriptor is not a ring.
            sys::IO_URING_ENTER if self.io_uring => {
                (-errno::EBADF, Resume::Emulate { ret: -errno::EBADF }, None)
            }
            // Without a ring, either call ends the run under `BOHR-2004`.
            sys::IO_URING_ENTER | sys::IO_URING_REGISTER => {
                let ret = self.unmodelled(ctx, nr, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            _ => {
                return Err(KernelError::Invariant(
                    "a call routed to a subsystem that does not answer it",
                ));
            }
        }))
    }
}
