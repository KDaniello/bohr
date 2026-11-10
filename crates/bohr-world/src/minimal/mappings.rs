//! Mappings: a file mapped into memory.

use super::*;

/// The calls this subsystem answers.
pub(super) const HANDLES: &[u32] = &[sys::MMAP, sys::MUNMAP, sys::MSYNC, sys::MADVISE];

impl MinimalWorld {
    /// Answers one of [`HANDLES`].
    pub(super) fn answer_mappings(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ctx: &mut StopCtx<'_, '_>,
    ) -> Result<Answer, KernelError> {
        Ok(Answer::Reply(match nr {
            // Only file mappings reach here.
            sys::MMAP => match self.map_file(ctx, args) {
                Some(resume) => {
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: false,
                        data_digest: None,
                    })?;
                    return Ok(Answer::Done(Effects::go(resume, SYSCALL_COST)));
                }
                // An unowned descriptor has no file.
                None => {
                    let ret = self.unmodelled(ctx, nr, args)?;
                    (ret, Resume::Emulate { ret }, None)
                }
            },
            // Let through; the model forgets the mapping.
            sys::MUNMAP => {
                self.unmap(ctx, args[0], args[1])?;
                (0, Resume::Continue, None)
            }
            // Nothing written through a mapping, so nothing to carry back. A cluster handles the
            // other case.
            sys::MSYNC => (0, Resume::Emulate { ret: 0 }, None),
            // Only `MADV_FREE` reaches here. A freed page keeps or loses its bytes by the host's
            // memory pressure; `MADV_DONTNEED`, which `MADV_FREE` permits, loses them on every
            // host, so the page reads zeros.
            // The advice is changed in its register at this stop, which the call then reads.
            sys::MADVISE => {
                /// `MADV_DONTNEED`.
                const DONTNEED: u64 = 4;
                let mut regs = ctx.regs()?;
                regs.args[2] = DONTNEED;
                ctx.set_regs(&regs)?;
                (0, Resume::Continue, None)
            }
            _ => {
                return Err(KernelError::Invariant(
                    "a call routed to a subsystem that does not answer it",
                ));
            }
        }))
    }
}
