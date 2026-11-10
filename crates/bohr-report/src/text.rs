//! What a run amounted to, in the few lines a terminal should print about it.

use std::fmt::Write as _;

use bohr_trace::{Event, EventKind, ExitStatus, Manifest};

use crate::describe;

/// Text from a run, made safe for a terminal.
///
/// Every control character but a newline and a tab is written as `\u{..}`: a bundle's strings,
/// a guest's output among them, cannot move the cursor, rewrite a line, or reach the clipboard.
#[must_use]
pub fn printable(text: &str) -> std::borrow::Cow<'_, str> {
    let unsafe_char = |c: char| c.is_control() && c != '\n' && c != '\t';
    if !text.chars().any(unsafe_char) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if unsafe_char(c) {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// One event on one line: where it happened, then what it was, then what it says.
///
/// A fixed shape people grep: the aligned envelope, the kind, then its fields. See
/// `docs/API.md` §4.3. Made [`printable`].
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
    printable(&out).into_owned()
}

/// The kinds `bohr trace --kind` selects by, one word per kind of event.
pub const KINDS: [&str; 13] = [
    "syscall",
    "schedule",
    "task",
    "packet",
    "disk",
    "fault",
    "node",
    "wait",
    "clock",
    "assertion",
    "record",
    "diagnostic",
    "domain",
];

/// The word an event's kind is selected by.
///
/// One word per kind and every kind has one, so a selection and its complement are the whole log.
#[must_use]
pub const fn kind_word(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Syscall { .. } => "syscall",
        EventKind::Schedule { .. } => "schedule",
        EventKind::TaskSpawned { .. } | EventKind::TaskExited { .. } => "task",
        EventKind::PacketSent { .. } | EventKind::PacketFate { .. } => "packet",
        EventKind::DiskOp { .. } => "disk",
        EventKind::FaultFired { .. } => "fault",
        EventKind::NodeState { .. } => "node",
        EventKind::Waited { .. } | EventKind::Woke { .. } => "wait",
        EventKind::ClockRead { .. } => "clock",
        EventKind::Assertion { .. } => "assertion",
        EventKind::SdkRecord { .. } => "record",
        EventKind::Diagnostic { .. } => "diagnostic",
        EventKind::Domain { .. } => "domain",
    }
}

/// What makes two events the same step of a loop: where, which kind, which call and its answer.
fn shape(event: &Event) -> (u32, u32, &'static str, u32, i64) {
    let (nr, ret) = match &event.kind {
        EventKind::Syscall { nr, ret, .. } => (*nr, *ret),
        _ => (0, 0),
    };
    (event.node.get(), event.task.get(), kind_word(&event.kind), nr, ret)
}

/// What one step of a loop is, in a word or two.
fn step_name(event: &Event) -> String {
    match &event.kind {
        EventKind::Syscall { nr, ret, .. } => {
            format!("{} → {ret}", crate::syscall_name(*nr).unwrap_or("?"))
        }
        other => kind_word(other).to_owned(),
    }
}

/// The log, one event per line, with a loop that repeats folded into two lines.
///
/// The two lines name the first and last event, the repeat count and the virtual time spanned;
/// `--json` keeps every event. Three repeats of up to four events on one task fold.
#[must_use]
pub fn write_squashed(events: &[Event], records: &[bohr_sdk_proto::Record]) -> String {
    let mut out = String::new();
    let mut at = 0;
    while at < events.len() {
        let mut folded = false;
        for period in 1..=4 {
            let mut times = 1;
            while at + (times + 1) * period <= events.len()
                && (0..period)
                    .all(|k| shape(&events[at + k]) == shape(&events[at + times * period + k]))
            {
                times += 1;
            }
            if times >= 3 {
                let first = &events[at];
                let last = &events[at + times * period - 1];
                let steps: Vec<String> = events[at..at + period].iter().map(step_name).collect();
                let _ = writeln!(
                    out,
                    "{:>8} t{:<14} n{} task{} ┐ {times} × [{}]",
                    first.id.get(),
                    first.tick.get(),
                    first.node.get(),
                    first.task.get(),
                    steps.join(", ")
                );
                let _ = writeln!(
                    out,
                    "{:>8} t{:<14} n{} task{} ┘ over {}",
                    last.id.get(),
                    last.tick.get(),
                    last.node.get(),
                    last.task.get(),
                    crate::narrate::seconds(last.tick.get().saturating_sub(first.tick.get()))
                );
                at += times * period;
                folded = true;
                break;
            }
        }
        if !folded {
            out.push_str(&write_log_with(&events[at..=at], records));
            at += 1;
        }
    }
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
/// The record's words are shown, since a probe judges by them.
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
/// Returned, so a report's header can reuse it.
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

#[cfg(test)]
mod squash_tests {
    use bohr_trace::{Event, EventKind};
    use bohr_types::{EventId, NodeId, TaskId, Tick};

    use super::*;

    /// A diagnostic a bundle carries reaches the terminal with its escape sequences written out:
    /// the clipboard sequence and a cursor move arrive as text.
    #[test]
    fn a_bundles_text_cannot_drive_the_terminal() {
        let message = "\u{1b}]52;c;cm0gLXJmIH4=\u{7}\u{1b}[2Aok".to_owned();
        let event = Event {
            id: EventId(1),
            tick: Tick(1),
            node: NodeId(0),
            task: TaskId(0),
            kind: EventKind::Diagnostic { code: "BOHR-2012".to_owned(), message },
        };
        let line = event_line(&event);
        assert!(!line.chars().any(|c| c.is_control() && c != '\n'), "a control reached: {line:?}");
        assert!(line.contains("\\u{1b}]52"), "the sequence is not shown as text: {line}");
        assert_eq!(printable("plain\ttext\n"), "plain\ttext\n");
    }

    fn call(id: u64, nr: u32) -> Event {
        Event {
            id: EventId(id),
            tick: Tick(id * 1000),
            node: NodeId(0),
            task: TaskId(1),
            kind: EventKind::Syscall {
                nr,
                args: [0; 6],
                ret: 0,
                emulated: true,
                data_digest: None,
            },
        }
    }

    /// A loop folds to two lines, and what is not a loop is printed as it is.
    #[test]
    fn a_loop_folds_and_the_rest_does_not() {
        let mut events = vec![call(0, 1)];
        for i in 0..30 {
            events.push(call(1 + i * 2, 228));
            events.push(call(2 + i * 2, 232));
        }
        events.push(call(100, 60));
        let out = write_squashed(&events, &[]);
        assert_eq!(out.lines().count(), 4, "{out}");
        assert!(out.contains("30 × [clock_gettime → 0, epoll_wait → 0]"), "{out}");
        assert!(KINDS.iter().all(|k| !k.is_empty()));
    }
}
