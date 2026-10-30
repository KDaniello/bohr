//! Freezing a reproduced run and giving it to a debugger.
//!
//! What is printed here is most of the feature. A hand-over that said only where to attach would
//! be answered within a day by somebody who ran `continue`, watched a syscall return nonsense and
//! filed it as a defect — so the limits the backend states are printed with the targets, every
//! time, rather than kept in a manual.

use bohr_backend::DebuggerTarget;

use crate::{execute::Target, CliError};

/// Prints where to attach, what will not work, and waits for the session to end.
///
/// Blocks on a line from the terminal. When it returns the caller drops the backend, which is what
/// ends the guests: they were released from the supervisor's lifetime so that a debugger could
/// hold them, and nothing else knows they exist.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code) // only a build with an interceptor ever has a guest to hand over
)]
pub(crate) fn conduct(targets: &[DebuggerTarget], stopped_at: u64, asked_for: u64) {
    if stopped_at == asked_for {
        eprintln!("stopped at event {stopped_at} · {} to attach to", targets.len());
    } else {
        // Never silently: the loop breaks between stops and one stop can record several events, so
        // the number reached is not always the number asked for, and a reader comparing the two
        // deserves to be told rather than to work it out.
        eprintln!(
            "stopped at event {stopped_at} · you asked for {asked_for} · {} to attach to",
            targets.len()
        );
    }

    // Attach lines to standard output so the last one can be piped somewhere; everything
    // explanatory to standard error, which is where the tool says things about a run.
    for target in targets {
        println!("node {}   {}", target.node().get(), target.command());
    }
    eprintln!();
    for limit in targets.first().map(DebuggerTarget::limits).unwrap_or_default() {
        eprintln!("  · {limit}");
    }

    eprintln!("\nPress Enter to end the session and kill the guest.");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// What a bundle with no guest process is told, wherever the hand-over is refused.
pub(crate) const NO_PROCESS: &str = "BOHR-3006: this bundle ran a guest built into the tool, so \
                                     there is no process for a debugger to attach to";

/// Refuses a hand-over for a bundle that never had a process to hand over.
///
/// Asked before the question about the host, because this one has the same answer on every host: a
/// reader told to relax a sysctl would relax it and be refused again for the reason that was true
/// all along.
pub(crate) fn refuse_if_there_is_no_process(target: &Target) -> Result<(), CliError> {
    match target {
        Target::Demo(..) => Err(CliError::Internal(NO_PROCESS.to_owned())),
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Target::Program(..) => Ok(()),
    }
}

/// Refuses a hand-over on a host where a debugger will not be allowed to attach.
///
/// Checked before replaying rather than after: the attach fails at the end of a run that took as
/// long as the original did, and learning it then costs the whole run.
///
/// This is a different question from the one `bohr doctor` asks about the same setting. Running a
/// guest needs *this process* to trace its own descendant, which the restricted mode permits. A
/// hand-over needs a debugger that is not an ancestor of anything to attach, which it does not.
pub(crate) fn refuse_if_a_debugger_cannot_attach() -> Result<(), CliError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let scope = bohr_backend_ptrace::doctor::sysctl("kernel.yama.ptrace_scope");
        if let Some(scope) = scope.filter(|value| value != "0") {
            return Err(CliError::Usage(format!(
                "BOHR-3001: a debugger cannot attach to the guest on this host\n  \
                 kernel.yama.ptrace_scope is {scope}, which permits tracing only a descendant of\n  \
                 the tracing process, and a debugger started by hand will not be one.\n  \
                 sudo sysctl -w kernel.yama.ptrace_scope=0     # this session only\n  \
                 Refused before replaying, because a replay that ends in a debugger that cannot\n  \
                 attach costs the whole run to learn nothing."
            )));
        }
    }
    Ok(())
}
