//! Signals: signals a task sends.

use super::*;

/// The calls this subsystem answers.
pub(super) const HANDLES: &[u32] = &[sys::TGKILL, sys::KILL];

impl MinimalWorld {
    /// Answers one of [`HANDLES`].
    pub(super) fn answer_signals(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ctx: &mut StopCtx<'_, '_>,
    ) -> Result<Answer, KernelError> {
        Ok(Answer::Reply(match nr {
            // A signal to another thread waits for its next stop; a thread parked in a held wait
            // is released with `EINTR`, as on Linux.
            sys::TGKILL if args[1] as u32 != Self::thread_id(ctx.task()) => {
                let target = TaskId(i64::from(args[1] as u32).wrapping_sub(GUEST_PID) as u32);
                let alive = self.born.contains(&target) && !self.ended.contains(&target);
                let signal = args[2] as u32;
                let ret = if !alive {
                    -errno::ESRCH
                } else if signal == 0 || signal > 64 {
                    if signal == 0 { 0 } else { -errno::EINVAL }
                } else {
                    self.pending_signals.entry(target).or_default().push(signal);
                    let interrupted = self.interrupt_wait(target);
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: true,
                        data_digest: None,
                    })?;
                    return Ok(Answer::Done(
                        Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST)
                            .releasing(interrupted),
                    ));
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // A fatal signal to itself ends the task, since `abort` retries until it dies.
            sys::TGKILL | sys::KILL => {
                let signal = if nr == sys::KILL { args[1] } else { args[2] } as u32;
                if FATAL_SIGNALS.contains(&signal) {
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: true,
                        data_digest: None,
                    })?;
                    return Ok(Answer::Done(Effects::go(
                        Resume::Terminate { signal: Some(signal) },
                        SYSCALL_COST,
                    )));
                }
                (0, Resume::Emulate { ret: 0 }, None)
            }
            _ => {
                return Err(KernelError::Invariant(
                    "a call routed to a subsystem that does not answer it",
                ));
            }
        }))
    }
}
