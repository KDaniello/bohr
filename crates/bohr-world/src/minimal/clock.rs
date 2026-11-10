//! Clock: the clock a guest reads and the sleeps it takes.

use super::*;

/// The calls this subsystem answers.
pub(super) const HANDLES: &[u32] =
    &[sys::CLOCK_GETTIME, sys::TIME, sys::GETTIMEOFDAY, sys::NANOSLEEP, sys::CLOCK_NANOSLEEP];

impl MinimalWorld {
    /// Answers one of [`HANDLES`].
    pub(super) fn answer_clock(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ctx: &mut StopCtx<'_, '_>,
    ) -> Result<Answer, KernelError> {
        Ok(Answer::Reply(match nr {
            sys::CLOCK_GETTIME => {
                let presented = if Self::reads_wall(args[0]) {
                    Self::presented_wall(ctx)
                } else {
                    Self::presented_now(ctx)
                };
                let d = Self::write_timespec(ctx, GuestAddr(args[1]), presented)?;
                ctx.emit(EventKind::ClockRead { presented })?;
                (0, Resume::Emulate { ret: 0 }, Some(d))
            }
            // Whole seconds, returned and optionally stored, recorded like any clock.
            sys::TIME => {
                let seconds = (Self::presented_wall(ctx).get() / Tick::SECOND.get()) as i64;
                let digest = if args[0] == 0 {
                    None
                } else {
                    let buf = seconds.to_le_bytes();
                    ctx.write_mem(GuestAddr(args[0]), &buf)?;
                    Some(short_digest(&buf))
                };
                let presented = Self::presented_wall(ctx);
                ctx.emit(EventKind::ClockRead { presented })?;
                (seconds, Resume::Emulate { ret: seconds }, digest)
            }
            sys::GETTIMEOFDAY => {
                let d = Self::write_timeval(ctx, GuestAddr(args[0]))?;
                let presented = Self::presented_wall(ctx);
                ctx.emit(EventKind::ClockRead { presented })?;
                (0, Resume::Emulate { ret: 0 }, Some(d))
            }
            // Sleeping costs virtual time only.
            sys::NANOSLEEP | sys::CLOCK_NANOSLEEP => {
                let request = if nr == sys::NANOSLEEP { args[0] } else { args[2] };
                let absolute = nr == sys::CLOCK_NANOSLEEP && args[1] & TIMER_ABSTIME != 0;
                let until = Self::read_timespec(ctx, GuestAddr(request))?;
                let ahead = if absolute {
                    until.saturating_sub(Self::deadline_clock(ctx, Self::reads_wall(args[0])).get())
                } else {
                    until
                };
                ctx.emit(EventKind::ClockRead { presented: Self::presented_now(ctx) })?;
                return Ok(Answer::Done(Effects::go(Resume::Emulate { ret: 0 }, Tick(ahead))));
            }
            _ => {
                return Err(KernelError::Invariant(
                    "a call routed to a subsystem that does not answer it",
                ));
            }
        }))
    }
}
