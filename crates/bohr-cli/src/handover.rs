//! Freezing a reproduced run and giving it to a debugger.
//!
//! The backend's limits are printed with every target, so nobody runs `continue` expecting
//! syscalls to work.

use bohr_backend::DebuggerTarget;

use crate::{CliError, execute::Target};

/// Prints where to attach, what will not work, and waits for the session to end.
///
/// Blocks on a line from the terminal. Then the caller drops the backend, which ends the guests.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code) // only a build with an interceptor ever has a guest to hand over
)]
pub(crate) fn conduct(targets: &[DebuggerTarget], stopped_at: u64, asked_for: u64) {
    if stopped_at == asked_for {
        eprintln!("stopped at event {stopped_at} · {} to attach to", targets.len());
    } else {
        // Said explicitly: the loop breaks between stops, and one stop can record several events,
        // so the event reached can pass the one asked for.
        eprintln!(
            "stopped at event {stopped_at} · you asked for {asked_for} · {} to attach to",
            targets.len()
        );
    }

    // Attach lines go to standard output for piping; explanations go to standard error.
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
/// Checked before the host, since this answer is the same on every host.
pub(crate) fn refuse_if_there_is_no_process(target: &Target) -> Result<(), CliError> {
    match target {
        Target::Demo(..) => Err(CliError::Internal(NO_PROCESS.to_owned())),
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Target::Program(..) => Ok(()),
    }
}

/// Refuses a hand-over on a host where a debugger will not be allowed to attach.
///
/// Checked before the replay, which can take as long as the original run.
///
/// `bohr doctor` checks a different thing: running a guest needs this process to trace its own
/// descendant, which restricted `ptrace_scope` allows. A debugger is not an ancestor, so it is not.
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
