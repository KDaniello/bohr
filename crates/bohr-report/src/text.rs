//! What a run amounted to, in the few lines a terminal should print about it.

use std::fmt::Write as _;

use bohr_trace::{Event, EventKind, ExitStatus, Manifest};

use crate::describe;

/// One event on one line: where it happened, then what it was, then what it says.
///
/// A promise once it exists — somebody will grep it — so the shape is fixed: the envelope first in
/// a width that keeps columns aligned for a whole run, then the kind, then whatever the kind
/// carries. See `docs/API.md` §4.3.
#[must_use]
pub fn event_line(event: &Event) -> String {
    let mut out = format!(
        "{:>8} t{:<14} n{} task{} ",
        event.id.get(),
        event.tick.get(),
        event.node.get(),
        event.task.get()
    );
    match &event.kind {
        EventKind::Syscall { nr, ret, emulated, .. } => {
            let name = crate::syscall_name(*nr).unwrap_or("?");
            let how = if *emulated { "emulated" } else { "passed" };
            let _ = write!(out, "syscall {name}({nr}) -> {ret} · {how}");
        }
        EventKind::Schedule { from, to, .. } => {
            let _ = write!(out, "schedule {} -> {}", from.get(), to.get());
        }
        EventKind::TaskSpawned { parent } => {
            let _ = write!(out, "task spawned by {}", parent.get());
        }
        EventKind::TaskExited { status } => {
            let _ = match status {
                ExitStatus::Exited(code) => write!(out, "task exited {code}"),
                ExitStatus::Signalled(signal) => write!(out, "task killed by signal {signal}"),
            };
        }
        EventKind::PacketSent { from, to, len, .. } => {
            let _ = write!(
                out,
                "packet sent {}:{} -> {}:{} · {len} bytes",
                from.node.get(),
                from.port,
                to.node.get(),
                to.port
            );
        }
        EventKind::PacketFate { fate, delay_ticks } => {
            let _ =
                write!(out, "packet {} after {delay_ticks} ticks", describe::packet_fate(*fate));
        }
        EventKind::DiskOp { fd, op, offset, len, ret } => {
            let _ = write!(
                out,
                "disk {} fd{fd} at {offset} · {len} bytes -> {ret}",
                describe::disk_op(*op)
            );
        }
        EventKind::FaultFired { fault, target } => {
            let _ = write!(
                out,
                "fault · {} · {}",
                describe::fault_action(*fault),
                describe::fault_target(target)
            );
        }
        EventKind::NodeState { state } => {
            let _ = write!(out, "node is now {}", describe::node_state(*state));
        }
        EventKind::Waited { on } => {
            let _ = write!(out, "waited on {on}");
        }
        EventKind::Woke { task, on } => {
            let _ = write!(out, "woke {} on {on}", task.get());
        }
        EventKind::ClockRead { presented } => {
            let _ = write!(out, "clock read as {}", presented.get());
        }
        EventKind::Assertion { name, kind, condition, .. } => {
            let held = if *condition { "held" } else { "DID NOT HOLD" };
            let _ =
                write!(out, "assertion {held} · {name} · {}", describe::assertion_promise(*kind));
        }
        EventKind::SdkRecord { index } => {
            let _ = write!(out, "record {index} from the guest");
        }
        EventKind::Diagnostic { code, message } => {
            let _ = write!(out, "diagnostic {code}: {message}");
        }
        EventKind::Domain { ns, kind, payload } => {
            let _ = write!(out, "domain event {kind} from {ns} · {} bytes", payload.len());
        }
    }
    out.push('\n');
    out
}

/// The whole log, one event per line.
#[must_use]
pub fn write_log(events: &[Event]) -> String {
    write_log_with(events, &[])
}

/// Longest stretch of a guest's record shown on its line; the rest is in the bundle.
const RECORD_SHOWN: usize = 240;

/// The whole log, each record from a guest followed by what it said.
///
/// What the record said, and not only that it was written, because a run judged by a probe is
/// judged by those words, and a reader asking why has nowhere else to look.
#[must_use]
pub fn write_log_with(events: &[Event], records: &[bohr_sdk_proto::Record]) -> String {
    let mut out = String::with_capacity(events.len() * 80);
    for event in events {
        let line = event_line(event);
        match &event.kind {
            EventKind::SdkRecord { index } if (*index as usize) < records.len() => {
                let said = records[*index as usize].to_line();
                let cut = said.char_indices().nth(RECORD_SHOWN).map_or(said.len(), |(at, _)| at);
                out.push_str(line.trim_end_matches('\n'));
                let _ = write!(out, " · {}", &said[..cut]);
                if cut < said.len() {
                    out.push('…');
                }
                out.push('\n');
            }
            _ => out.push_str(&line),
        }
    }
    out
}

/// The summary a command prints when a bundle turned out to be what it said it was.
///
/// Returned rather than printed: the same lines belong in a report's header, and a function that
/// wrote to standard output could not be reused there without the report growing a terminal.
#[must_use]
pub fn summary(manifest: &Manifest) -> String {
    let counters = &manifest.counters;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  seed {} · {} · written {}",
        manifest.seed,
        describe::outcome_word(&manifest.outcome),
        manifest.created_utc
    );
    let _ = writeln!(
        out,
        "  {} events · {} syscalls · {} draws · {} tasks on {} nodes",
        counters.events, counters.syscalls, counters.draws, counters.tasks, counters.nodes
    );
    let _ = writeln!(
        out,
        "  {} under {}/{} on {}",
        manifest.policy.scheduler,
        manifest.provenance.world_model,
        manifest.provenance.oracle,
        manifest.provenance.backend
    );
    if manifest.determinism.level != "full" {
        let _ = writeln!(out, "  degraded: {}", manifest.determinism.reasons.join(", "));
    }
    out
}
