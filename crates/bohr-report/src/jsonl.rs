//! The event log as JSON Lines: one object per event, one event per line.
//!
//! `bohr trace --format json` and a windowed report's sidecar write the same bytes.
//!
//! Every line has the envelope `id`, `tick`, `node`, `task`, `kind`, with the variant's fields
//! under a key named by `kind`, so an unknown event can still be placed.
//!
//! Addresses, registers and digests are hex strings, since `JSON.parse` loses precision above 2^53.
//! Counts and ids stay numbers.
//!
//! This lives here, apart from the canonical encodings in `bohr-trace`, so nothing digests it.

use bohr_trace::{
    AssertionKind, DiskOpKind, Endpoint, Event, EventKind, ExitStatus, FaultKind, FaultTarget,
    PacketFate, SwitchReason,
};

/// Writes one event as a single line, including the newline.
pub fn write_event(event: &Event, out: &mut String) {
    let kind = kind_name(&event.kind);
    out.push('{');
    number(out, "id", event.id.get());
    out.push(',');
    number(out, "tick", event.tick.get());
    out.push(',');
    number(out, "node", u64::from(event.node.get()));
    out.push(',');
    number(out, "task", u64::from(event.task.get()));
    out.push(',');
    text(out, "kind", kind);
    out.push(',');
    key(out, kind);
    body(&event.kind, out);
    out.push_str("}\n");
}

/// Writes a whole log.
#[must_use]
pub fn write_log(events: &[Event]) -> String {
    // Roughly one event's size, to avoid re-allocating per line.
    let mut out = String::with_capacity(events.len() * 160);
    for event in events {
        write_event(event, &mut out);
    }
    out
}

/// The name a `kind` field carries, in the snake case the rest of the format uses.
///
/// Exhaustive with no wildcard, so a new event kind fails to compile until it is projected.
const fn kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Syscall { .. } => "syscall",
        EventKind::Schedule { .. } => "schedule",
        EventKind::TaskSpawned { .. } => "task_spawned",
        EventKind::TaskExited { .. } => "task_exited",
        EventKind::PacketSent { .. } => "packet_sent",
        EventKind::PacketFate { .. } => "packet_fate",
        EventKind::DiskOp { .. } => "disk_op",
        EventKind::FaultFired { .. } => "fault_fired",
        EventKind::NodeState { .. } => "node_state",
        EventKind::Waited { .. } => "waited",
        EventKind::Woke { .. } => "woke",
        EventKind::ClockRead { .. } => "clock_read",
        EventKind::Assertion { .. } => "assertion",
        EventKind::SdkRecord { .. } => "sdk_record",
        EventKind::Diagnostic { .. } => "diagnostic",
        EventKind::Domain { .. } => "domain",
    }
}

/// The variant's own fields, as an object.
#[allow(clippy::too_many_lines)] // one arm per kind of event, and each is three lines of writing
fn body(kind: &EventKind, out: &mut String) {
    out.push('{');
    match kind {
        EventKind::Syscall { nr, args, ret, emulated, data_digest } => {
            number(out, "nr", u64::from(*nr));
            if let Some(name) = crate::syscall_name(*nr) {
                out.push(',');
                text(out, "name", name);
            }
            out.push(',');
            key(out, "args");
            out.push('[');
            for (at, arg) in args.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                hex(out, *arg);
            }
            out.push_str("],");
            signed(out, "ret", *ret);
            out.push(',');
            boolean(out, "emulated", *emulated);
            if let Some(digest) = data_digest {
                out.push(',');
                text(out, "data_digest", &bytes_as_hex(digest));
            }
        }
        EventKind::Schedule { from, to, reason } => {
            number(out, "from", u64::from(from.get()));
            out.push(',');
            number(out, "to", u64::from(to.get()));
            out.push(',');
            text(out, "reason", switch_reason(*reason));
        }
        EventKind::TaskSpawned { parent } => number(out, "parent", u64::from(parent.get())),
        EventKind::TaskExited { status } => match status {
            ExitStatus::Exited(code) => {
                text(out, "how", "exited");
                out.push(',');
                signed(out, "code", i64::from(*code));
            }
            ExitStatus::Signalled(signal) => {
                text(out, "how", "signalled");
                out.push(',');
                number(out, "signal", u64::from(*signal));
            }
        },
        EventKind::PacketSent { from, to, len, digest } => {
            key(out, "from");
            endpoint(from, out);
            out.push(',');
            key(out, "to");
            endpoint(to, out);
            out.push(',');
            number(out, "len", u64::from(*len));
            out.push(',');
            text(out, "digest", &bytes_as_hex(digest));
        }
        EventKind::PacketFate { fate, delay_ticks } => {
            text(out, "fate", packet_fate(*fate));
            out.push(',');
            number(out, "delay_ticks", *delay_ticks);
        }
        EventKind::DiskOp { fd, op, offset, len, ret } => {
            signed(out, "fd", i64::from(fd.0));
            out.push(',');
            text(out, "op", disk_op(*op));
            out.push(',');
            number(out, "offset", *offset);
            out.push(',');
            number(out, "len", u64::from(*len));
            out.push(',');
            signed(out, "ret", *ret);
        }
        EventKind::FaultFired { fault, target } => {
            text(out, "fault", fault_kind(*fault));
            out.push(',');
            key(out, "target");
            fault_target(target, out);
        }
        EventKind::NodeState { state } => text(out, "state", crate::describe::node_state(*state)),
        EventKind::Waited { on } => number(out, "on", u64::from(on.get())),
        EventKind::Woke { task, on } => {
            number(out, "task", u64::from(task.get()));
            out.push(',');
            number(out, "on", u64::from(on.get()));
        }
        EventKind::ClockRead { presented } => number(out, "presented", presented.get()),
        EventKind::Assertion { name, kind, hit, condition } => {
            text(out, "name", name);
            out.push(',');
            text(out, "promise", assertion_kind(*kind));
            out.push(',');
            boolean(out, "hit", *hit);
            out.push(',');
            boolean(out, "condition", *condition);
        }
        EventKind::SdkRecord { index } => number(out, "index", u64::from(*index)),
        EventKind::Diagnostic { code, message } => {
            text(out, "code", code);
            out.push(',');
            text(out, "message", message);
        }
        EventKind::Domain { ns, kind, payload } => {
            text(out, "ns", &ns.to_string());
            out.push(',');
            number(out, "kind", u64::from(*kind));
            out.push(',');
            text(out, "payload", &bytes_as_hex(payload));
        }
    }
    out.push('}');
}

fn endpoint(endpoint: &Endpoint, out: &mut String) {
    out.push('{');
    number(out, "node", u64::from(endpoint.node.get()));
    out.push(',');
    number(out, "port", u64::from(endpoint.port));
    out.push('}');
}

fn fault_target(target: &FaultTarget, out: &mut String) {
    out.push('{');
    match target {
        FaultTarget::Node(node) => {
            text(out, "what", "node");
            out.push(',');
            number(out, "node", u64::from(node.get()));
        }
        FaultTarget::Link { from, to } => {
            text(out, "what", "link");
            out.push(',');
            number(out, "from", u64::from(from.get()));
            out.push(',');
            number(out, "to", u64::from(to.get()));
        }
        FaultTarget::Descriptor(fd) => {
            text(out, "what", "descriptor");
            out.push(',');
            signed(out, "fd", i64::from(fd.0));
        }
        FaultTarget::World => text(out, "what", "world"),
    }
    out.push('}');
}

const fn switch_reason(reason: SwitchReason) -> &'static str {
    match reason {
        SwitchReason::SyscallBoundary => "syscall_boundary",
        SwitchReason::Blocked => "blocked",
        SwitchReason::BudgetExhausted => "budget_exhausted",
        SwitchReason::Yield => "yield",
        SwitchReason::TaskExit => "task_exit",
        SwitchReason::FaultPoint => "fault_point",
    }
}

const fn packet_fate(fate: PacketFate) -> &'static str {
    match fate {
        PacketFate::Delivered => "delivered",
        PacketFate::Dropped => "dropped",
        PacketFate::Duplicated => "duplicated",
        PacketFate::Reordered => "reordered",
        PacketFate::Corrupted => "corrupted",
    }
}

const fn disk_op(op: DiskOpKind) -> &'static str {
    match op {
        DiskOpKind::Read => "read",
        DiskOpKind::Write => "write",
        DiskOpKind::Fsync => "fsync",
        DiskOpKind::Truncate => "truncate",
    }
}

const fn assertion_kind(kind: AssertionKind) -> &'static str {
    match kind {
        AssertionKind::Always => "always",
        AssertionKind::AlwaysOrUnreachable => "always_or_unreachable",
        AssertionKind::Sometimes => "sometimes",
        AssertionKind::Reachable => "reachable",
        AssertionKind::Unreachable => "unreachable",
    }
}

const fn fault_kind(fault: FaultKind) -> &'static str {
    fault.name()
}

fn bytes_as_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Written straight into the buffer; this runs per payload byte.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn key(out: &mut String, name: &str) {
    out.push('"');
    out.push_str(name);
    out.push_str("\":");
}

/// A string value, escaped by the JSON library.
///
/// These strings come from the guest and may hold quotes or control characters.
fn text(out: &mut String, name: &str, value: &str) {
    key(out, name);
    match serde_json::to_string(value) {
        Ok(escaped) => out.push_str(&escaped),
        Err(_) => out.push_str("\"\""),
    }
}

fn number(out: &mut String, name: &str, value: u64) {
    key(out, name);
    out.push_str(&value.to_string());
}

fn signed(out: &mut String, name: &str, value: i64) {
    key(out, name);
    out.push_str(&value.to_string());
}

fn boolean(out: &mut String, name: &str, value: bool) {
    key(out, name);
    out.push_str(if value { "true" } else { "false" });
}

/// A number too wide to survive being read as a double, written as a string that is.
fn hex(out: &mut String, value: u64) {
    use std::fmt::Write as _;
    out.push('"');
    let _ = write!(out, "{value:#x}");
    out.push('"');
}
