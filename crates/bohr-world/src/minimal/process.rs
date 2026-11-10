//! Process: what a process is told about itself: its identity, its limits, its scheduling, the
//! randomness it draws, and how it ends.

use super::*;

/// The calls this subsystem answers.
pub(super) const HANDLES: &[u32] = &[
    sys::GETRANDOM,
    sys::UNAME,
    sys::GETUID,
    sys::GETGID,
    sys::GETEUID,
    sys::GETEGID,
    sys::GETPPID,
    sys::GETPGRP,
    sys::SCHED_GETPARAM,
    sys::SCHED_GETSCHEDULER,
    sys::SYSINFO,
    sys::GETRUSAGE,
    sys::GETRLIMIT,
    sys::SETRLIMIT,
    sys::PRLIMIT64,
    sys::SCHED_GETAFFINITY,
    sys::SCHED_YIELD,
    sys::GETPRIORITY,
    sys::SETPRIORITY,
    sys::SCHED_SETSCHEDULER,
    sys::SCHED_SETPARAM,
    sys::PRCTL,
    sys::GETPID,
    sys::GETTID,
    sys::EXIT,
    sys::EXIT_GROUP,
    sys::RSEQ,
    sys::GETCPU,
];

impl MinimalWorld {
    /// Answers one of [`HANDLES`].
    pub(super) fn answer_process(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ctx: &mut StopCtx<'_, '_>,
    ) -> Result<Answer, KernelError> {
        Ok(Answer::Reply(match nr {
            sys::GETRANDOM => {
                // Bounded; a larger request gets a short answer.
                let len = usize::try_from(args[1]).unwrap_or(0).min(MAX_TRANSFER);
                let mut bytes = vec![0u8; len];
                ctx.draw_into(&mut bytes, DrawSite::GuestRandom);
                ctx.write_mem(GuestAddr(args[0]), &bytes)?;
                (len as i64, Resume::Emulate { ret: len as i64 }, Some(short_digest(&bytes)))
            }
            sys::UNAME => {
                let ret = Self::write_uname(ctx, GuestAddr(args[0]))?;
                (ret, Resume::Emulate { ret }, None)
            }
            // One fixed identity on every machine.
            sys::GETUID
            | sys::GETGID
            | sys::GETEUID
            | sys::GETEGID
            | sys::GETPPID
            | sys::GETPGRP
            | sys::SCHED_GETPARAM
            | sys::SCHED_GETSCHEDULER => (0, Resume::Emulate { ret: 0 }, None),
            // No memory figures, so nothing to report.
            // Zero-filled, so no stack contents leak through.
            sys::SYSINFO => {
                ctx.write_mem(GuestAddr(args[0]), &[0u8; SYSINFO_BYTES])?;
                (0, Resume::Emulate { ret: 0 }, None)
            }
            // No per-task accounting, so every field is zero.
            sys::GETRUSAGE => {
                ctx.write_mem(GuestAddr(args[1]), &[0u8; RUSAGE_BYTES])?;
                (0, Resume::Emulate { ret: 0 }, None)
            }
            // Fixed limits. `prlimit64` takes its resource one argument along; a new limit is
            // accepted and ignored.
            sys::GETRLIMIT | sys::SETRLIMIT | sys::PRLIMIT64 => {
                let (resource, out) = match nr {
                    sys::GETRLIMIT => (args[0], args[1]),
                    sys::SETRLIMIT => (args[0], 0),
                    _ => (args[1], args[3]),
                };
                if out != 0 {
                    let (soft, hard) = limit_for(resource);
                    let mut answer = [0u8; RLIMIT_BYTES];
                    answer[..8].copy_from_slice(&soft.to_le_bytes());
                    answer[8..].copy_from_slice(&hard.to_le_bytes());
                    ctx.write_mem(GuestAddr(out), &answer)?;
                }
                (0, Resume::Emulate { ret: 0 }, None)
            }
            // One processor, since one task runs at a time.
            sys::SCHED_GETAFFINITY => {
                let ret = Self::write_affinity(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // A yield is a switch point the guest names.
            sys::SCHED_YIELD => {
                ctx.emit(EventKind::Syscall {
                    nr,
                    args,
                    ret: 0,
                    emulated: true,
                    data_digest: None,
                })?;
                return Ok(Answer::Done(
                    Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST).yielding(),
                ));
            }
            // Niceness 0: the raw call returns 20 minus niceness.
            sys::GETPRIORITY => (20, Resume::Emulate { ret: 20 }, None),
            // Setting it is accepted and ignored; the tape schedules.
            sys::SETPRIORITY => (0, Resume::Emulate { ret: 0 }, None),
            // As for an unprivileged process: ordinary policies are accepted, real-time ones are
            // `EPERM`.
            sys::SCHED_SETSCHEDULER => {
                /// `SCHED_RESET_ON_FORK`, a flag ORed into the policy.
                const RESET_ON_FORK: u64 = 0x4000_0000;
                let mut priority = [0u8; 4];
                ctx.read_mem(GuestAddr(args[2]), &mut priority)?;
                let ret = match args[1] & !RESET_ON_FORK {
                    // SCHED_OTHER, SCHED_BATCH, SCHED_IDLE, whose one priority is zero.
                    0 | 3 | 5 if i32::from_le_bytes(priority) == 0 => 0,
                    0 | 3 | 5 => -errno::EINVAL,
                    // SCHED_FIFO, SCHED_RR, SCHED_DEADLINE.
                    1 | 2 | 6 => -errno::EPERM,
                    _ => -errno::EINVAL,
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // The ordinary policy's only priority is zero; others are `EINVAL`.
            sys::SCHED_SETPARAM => {
                let mut priority = [0u8; 4];
                ctx.read_mem(GuestAddr(args[1]), &mut priority)?;
                let ret = if i32::from_le_bytes(priority) == 0 { 0 } else { -errno::EINVAL };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::PRCTL => {
                let ret = self.prctl(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // One fixed pid.
            sys::GETPID => (GUEST_PID, Resume::Emulate { ret: GUEST_PID }, None),
            // The run's own task id.
            sys::GETTID => {
                let ret = i64::from(ctx.task().get()) + GUEST_PID;
                (ret, Resume::Emulate { ret }, None)
            }
            // Refused as by a kernel without it, so the host kernel never writes the worker's core
            // into the guest's memory; glibc then asks `getcpu`.
            sys::RSEQ => (-errno::ENOSYS, Resume::Emulate { ret: -errno::ENOSYS }, None),
            // One processor and one memory node, both nought.
            sys::GETCPU => {
                for at in [args[0], args[1]] {
                    if at != 0 {
                        ctx.write_mem(GuestAddr(at), &0u32.to_le_bytes())?;
                    }
                }
                (0, Resume::Emulate { ret: 0 }, None)
            }
            // Let through; the exit arrives as its own stop.
            sys::EXIT | sys::EXIT_GROUP => (0, Resume::Continue, None),
            _ => {
                return Err(KernelError::Invariant(
                    "a call routed to a subsystem that does not answer it",
                ));
            }
        }))
    }
}
