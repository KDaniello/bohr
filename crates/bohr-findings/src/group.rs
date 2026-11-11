//! What tells one finding from another: the programs a run executed, its violation's signature, and
//! where it fell.
//!
//! One defect reached from many seeds gives many bundles; grouped by this, they read as defects.
//! Grouping, ablation and reduction all compare by [`finding_key`].

use bohr_reducer::Violation;
use bohr_trace::{Bundle, EventKind, Outcome};

/// The signature a bundle's violation is grouped by.
///
/// A hang is named by its words with digits removed, since the manifest lacks the stop reason:
/// "event budget reached at 1000000" and "at 1000001" are the same hang.
pub fn signature_of(outcome: &Outcome, events: &[bohr_trace::Event]) -> String {
    if let Outcome::GuestHung(said) = outcome {
        let words: String = said.chars().filter(|c| !c.is_ascii_digit()).collect();
        return format!("GuestHung {}", words.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    // A law is named by its code, its law and the words of its first message: the heights, nodes,
    // keys and hashes in them differ between runs of one defect.
    if let Outcome::DurabilityViolated(detail) | Outcome::ProbeViolated(detail) = outcome
        && let Some((code, rest)) = detail.split_once(": ")
        && bohr_trace::LAW_CODES.contains(&code)
        && let Some((law, said)) = rest.split_once(": ")
        && law.starts_with('R')
    {
        let first = said.split("; ").next().unwrap_or(said);
        let words: Vec<&str> =
            first.split_whitespace().filter(|w| !w.chars().any(|c| c.is_ascii_digit())).collect();
        return format!("{code} {law} {}", words.join(" "));
    }
    Violation::of(outcome, bohr_kernel::StopReason::GuestFinished, events).map_or_else(
        || {
            let verdict = bohr_report::narrate::verdict(outcome);
            format!("{}: {}", verdict.code, verdict.headline)
        },
        |v| v.describe(),
    )
}

/// The programs a run executed, by file name, in the order the run started them.
pub fn programs_of(records: &[bohr_trace::BinaryRecord]) -> String {
    let mut names: Vec<String> = Vec::new();
    // The programs under test; the probe is the bench's own.
    for record in records.iter().filter(|r| r.role.under_test()) {
        let name = record.path.rsplit('/').next().unwrap_or(&record.path).to_owned();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() { "?".to_owned() } else { names.join("+") }
}

/// Whether an event is one a run's group is read from: how tasks exited, what was asserted and
/// diagnosed, and what the world did to the nodes. The rest is the bulk of a log.
pub const fn telling(event: &bohr_trace::Event) -> bool {
    matches!(
        event.kind,
        EventKind::TaskExited { .. }
            | EventKind::Assertion { .. }
            | EventKind::Diagnostic { .. }
            | EventKind::FaultFired { .. }
            | EventKind::NodeState { .. }
    )
}

/// The group of a run: the programs it ran, and its [`finding_key`].
pub fn group_of(
    programs: &str,
    outcome: &Outcome,
    events: &[bohr_trace::Event],
    tails: &[bohr_trace::OutputTail],
) -> String {
    format!("{programs}: {}", finding_key(outcome, events, tails))
}

/// What tells one finding from another: its violation's signature, and where it fell.
///
/// The place splits one crash class into its defects: two panics at two frames are two findings,
/// though both exit 2. Grouping, ablation and reduction all compare by this, so a trial that fell
/// elsewhere is another finding. A run that kept no output is told apart by its signature alone.
pub fn finding_key(
    outcome: &Outcome,
    events: &[bohr_trace::Event],
    tails: &[bohr_trace::OutputTail],
) -> String {
    let base = signature_of(outcome, events);
    match place_of(events, tails) {
        Some(place) => format!("{base} @ {place}"),
        None => base,
    }
}

/// Where a run fell: where a broken law's lost acknowledgement was made, where a law names one,
/// otherwise read from what its failing nodes printed last.
///
/// A law is told by its record, never by a log line, so a lost write is placed at the code that
/// acknowledged it.
pub fn place_of(events: &[bohr_trace::Event], tails: &[bohr_trace::OutputTail]) -> Option<String> {
    // The law the oracle tells a finding by is the first of `LAW_CODES` present.
    // A law's message names its law first, which tells it from a probe's of the same code.
    let by_law = |code: &str, message: &str| {
        bohr_trace::LAW_CODES.contains(&code) && message.starts_with('R')
    };
    let told = bohr_trace::LAW_CODES.iter().find(|law| {
        events.iter().any(|e| {
            matches!(&e.kind, bohr_trace::EventKind::Diagnostic { code, message }
                if code == *law && by_law(code, message))
        })
    });
    let acknowledged = told.and_then(|law| {
        events.iter().find_map(|event| match &event.kind {
            bohr_trace::EventKind::Diagnostic { code, message } if code == *law => {
                let at = message.rsplit_once(", acknowledged at ")?.1;
                Some(format!("acknowledged at {at}"))
            }
            _ => None,
        })
    });
    if acknowledged.is_some() {
        return acknowledged;
    }
    let failing = bohr_report::narrate::failing_nodes(events);
    bohr_report::locate::locate(tails, &failing).map(|place| place.signature())
}

/// What a bundle is grouped by; see [`group_of`].
///
/// # Errors
///
/// A section the group is read from does not read, so the bundle cannot be grouped.
pub fn signature(
    bundle: &Bundle,
    outcome: &Outcome,
    events: &[bohr_trace::Event],
) -> Result<String, bohr_trace::TraceError> {
    let records = bundle.binaries()?;
    Ok(group_of(&programs_of(&records), outcome, events, &bundle.output_tails()?))
}

/// A bundle's [`telling`] events; none where it holds no log.
///
/// A log that does not read to its recorded hash is refused, never grouped as a run of no events.
pub fn telling_events(bundle: &Bundle) -> Result<Vec<bohr_trace::Event>, bohr_trace::TraceError> {
    Ok(bundle
        .event_log()?
        .map(|raw| bohr_trace::read_events_where(raw, telling).0)
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use bohr_trace::{Event, EventKind, ExitStatus, OutputTail};
    use bohr_types::{EventId, NodeId, TaskId, Tick};

    use super::*;

    /// Node 0 ends with status 2 after printing `said`, as every Go panic ends.
    fn panicked(said: &str) -> (Vec<Event>, Vec<OutputTail>) {
        let exit = Event {
            id: EventId(1),
            tick: Tick(1),
            node: NodeId(0),
            task: TaskId(0),
            kind: EventKind::TaskExited { status: ExitStatus::Exited(2) },
        };
        let tail = OutputTail {
            node: 0,
            incarnation: 0,
            started_tick: 0,
            dropped: 0,
            text: said.to_owned(),
        };
        (vec![exit], vec![tail])
    }

    const FRAME_A: &str = "panic: runtime error: index out of range\n\ngoroutine 1 [running]:\n\
                           main.apply(...)\n\t/src/raft/log.go:120 +0x1d\n";
    const FRAME_B: &str = "panic: runtime error: index out of range\n\ngoroutine 1 [running]:\n\
                           main.replay(...)\n\t/src/raft/wal.go:88 +0x2f\n";

    /// A group read from the telling events alone is the group the whole log gives, so `bohr
    /// findings` decodes the bulk of a log without keeping it.
    #[test]
    fn the_telling_events_name_the_same_group_as_the_whole_log() {
        let (exit, tails) = panicked(FRAME_A);
        let syscall = |id| Event {
            id: EventId(id),
            tick: Tick(id),
            node: NodeId(0),
            task: TaskId(0),
            kind: EventKind::Syscall {
                nr: 1,
                args: [0; 6],
                ret: 0,
                emulated: true,
                data_digest: None,
            },
        };
        let whole = vec![syscall(0), exit[0].clone(), syscall(2)];
        let kept: Vec<Event> = whole.iter().filter(|e| telling(e)).cloned().collect();
        assert_eq!(kept.len(), 1, "a system call is bulk");
        let outcome = Outcome::GuestCrashed("exit status 2".to_owned());
        assert_eq!(finding_key(&outcome, &kept, &tails), finding_key(&outcome, &whole, &tails));
        // The reducer reads a candidate the same way, so its violation and place agree too.
        let stop = bohr_kernel::StopReason::GuestFinished;
        assert_eq!(Violation::of(&outcome, stop, &kept), Violation::of(&outcome, stop, &whole));
        assert_eq!(place_of(&kept, &tails), place_of(&whole, &tails));
    }

    /// A run's programs are the ones under test; the probe that asked them is not one.
    #[test]
    fn the_probe_is_not_a_program_under_test() {
        let record = |path: &str, role: bohr_trace::BinaryRole| bohr_trace::BinaryRecord {
            path: path.to_owned(),
            blake3: String::new(),
            role,
            args: Vec::new(),
        };
        let records = [
            record("/opt/geth", bohr_trace::BinaryRole::Executable),
            record("/tmp/bohr-probe-1", bohr_trace::BinaryRole::Probe),
        ];
        assert_eq!(programs_of(&records), "geth");
    }

    /// Two panics with one exit status at two frames are two findings; one frame twice is one.
    #[test]
    fn a_finding_is_told_apart_by_where_it_fell() {
        let outcome = Outcome::GuestCrashed("exit status 2".to_owned());
        let key = |said: &str| {
            let (events, tails) = panicked(said);
            finding_key(&outcome, &events, &tails)
        };
        assert_eq!(key(FRAME_A), key(FRAME_A));
        assert_ne!(key(FRAME_A), key(FRAME_B), "two frames read as one finding");
        assert!(key(FRAME_A).contains("log.go"), "the place is missing: {}", key(FRAME_A));
    }

    #[test]
    fn one_law_broken_at_other_heights_and_nodes_is_one_group() {
        let at = |node: u32, from: u32, to: u32| {
            Outcome::DurabilityViolated(format!(
                "BOHR-2055: R3.2: node {node} reached position {from} within 10000000000 ticks of                  the fault window closing, where position {to} was decided before it closed"
            ))
        };
        assert_eq!(signature_of(&at(3, 5, 14), &[]), signature_of(&at(1, 12, 17), &[]));
        assert!(signature_of(&at(3, 5, 14), &[]).starts_with("BOHR-2055 R3.2 node reached"));
        let hashes = |a: &str| {
            Outcome::ProbeViolated(format!(
                "BOHR-2041: R2.1: position 25 decided as {a} by node 0 and as 9F by node 1"
            ))
        };
        assert_eq!(signature_of(&hashes("23AB"), &[]), signature_of(&hashes("77CD"), &[]));
    }
}
