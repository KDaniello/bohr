//! A stored bundle, digested once into the shape every renderer asks it for.
//!
//! Each output — text, JSON Lines, a page — needs the same joins performed: the failed assertion
//! tied to the record that says where it is written, the draws tied to the events they belong to,
//! the events grouped by the node that recorded them. Doing them per renderer is how two outputs
//! of one bundle end up disagreeing, so they happen here, once, and every renderer reads the
//! result.
//!
//! Absence is not an error. A bundle from a guest with no SDK has no records; one from a run with
//! no coverage runtime has no map. A model that refused those would refuse most real bundles.

use std::collections::BTreeMap;

use bohr_sdk_proto::{AssertRecord, Record};
use bohr_tape::{DrawRecord, Journal, DRAW_RECORD_BYTES};
use bohr_trace::{
    read_events_lossy, BinaryRecord, Event, EventKind, HostProfile, Manifest, SectionId,
};
use bohr_types::{EventId, NodeId};

use crate::ReportError;

/// Everything about one run that a renderer is entitled to.
#[derive(Clone, Debug)]
pub struct RunModel {
    /// What the run says about itself without being re-executed.
    pub manifest: Manifest,
    /// The decoded timeline, in the order it was recorded.
    pub events: Vec<Event>,
    /// What the run drew, and where.
    pub journal: Journal,
    /// What the guests wrote about themselves, parsed.
    ///
    /// A line this build has no case for survives as [`Record::Unknown`] rather than being
    /// dropped: a bundle written by a newer tool is still worth reading.
    pub records: Vec<Record>,
    /// What the guest was shown of the machine, where the bundle carries it.
    pub host: Option<HostProfile>,
    /// The objects the run started with.
    pub binaries: Vec<BinaryRecord>,
    /// The effective configuration, verbatim.
    pub config: String,
    /// Whatever anybody wrote on the bundle afterwards.
    pub notes: Vec<String>,
    /// Where a promise broke, and which promise.
    pub violation: Option<Violation>,
    /// How far the log could be read before it stopped making sense, where it did.
    ///
    /// A truncated log is worth showing up to the truncation — a supervisor killed mid-write still
    /// recorded everything before it — and worth saying so about, because a reader who is not told
    /// will read the last event as the end of the run.
    pub truncated: Option<String>,
}

/// A promise the guest made about itself and did not keep.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Violation {
    /// The event where it failed.
    pub event: EventId,
    /// Which node was running.
    pub node: NodeId,
    /// What the guest called it.
    pub name: String,
    /// What kind of promise it was.
    pub kind: bohr_trace::AssertionKind,
    /// Where it is written, from the record the guest wrote. Absent where the bundle carries no
    /// records, which is every bundle from a guest built without the SDK.
    pub location: Option<Location>,
}

/// Where in the guest's own source a promise is written.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Location {
    /// Source file, as the guest's compiler named it.
    pub file: String,
    /// Function it sits in.
    pub function: String,
    /// Line it sits on.
    pub line: u32,
}

impl RunModel {
    /// Digests a parsed bundle.
    ///
    /// # Errors
    ///
    /// Returns an error only where the bundle lacks something no reader can do without: the
    /// manifest, or an event log that is not a log at all.
    pub fn of(bundle: &bohr_trace::Bundle) -> Result<Self, ReportError> {
        let manifest = bundle.manifest().map_err(ReportError::Unreadable)?;
        // Asked for rather than assumed: a report of one execution built from a bundle holding
        // several would describe a run nobody made, and would look exactly like a report.
        let single = bundle.single().map_err(ReportError::Unreadable)?;

        let (events, damage) = match single.events() {
            Some(bytes) => read_events_lossy(bytes),
            None => (Vec::new(), None),
        };
        let journal =
            bundle.section(SectionId::DrawJournal).map_or_else(Journal::new, read_journal);
        let records = single.sdk_records().map(read_records).unwrap_or_default();
        let host =
            bundle.section(SectionId::HostProfile).and_then(|b| serde_json::from_slice(b).ok());
        let binaries = bundle
            .section(SectionId::Binaries)
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default();
        let config = bundle
            .section(SectionId::Config)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let notes = bundle
            .section(SectionId::Notes)
            .map(|b| String::from_utf8_lossy(b).lines().map(str::to_owned).collect())
            .unwrap_or_default();

        let violation = find_violation(&events, &records);
        Ok(Self {
            manifest,
            events,
            journal,
            records,
            host,
            binaries,
            config,
            notes,
            violation,
            truncated: damage.map(|e| e.to_string()),
        })
    }

    /// The nodes that recorded anything, in order.
    #[must_use]
    pub fn nodes(&self) -> Vec<NodeId> {
        let mut seen: Vec<NodeId> = self.events.iter().map(|e| e.node).collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    }

    /// The draws each event made, by event.
    ///
    /// The join the draw journal exists for: a record carries the event current when it was drawn,
    /// so "what decided this" is a lookup rather than a guess about ordering.
    #[must_use]
    pub fn draws_by_event(&self) -> BTreeMap<EventId, Vec<&DrawRecord>> {
        let mut by_event: BTreeMap<EventId, Vec<&DrawRecord>> = BTreeMap::new();
        for record in self.journal.records() {
            by_event.entry(EventId(record.event)).or_default().push(record);
        }
        by_event
    }

    /// The assertions the guests stated, with how each turned out.
    #[must_use]
    pub fn assertions(&self) -> Vec<&AssertRecord> {
        self.records
            .iter()
            .filter_map(|r| match r {
                Record::Assert(assertion, _) => Some(assertion),
                _ => None,
            })
            .collect()
    }
}

/// Reads a draw journal section back, ignoring a trailing partial record.
///
/// The section is a flat array of fixed-width records, so a short tail is damage rather than a
/// shorter journal — and a report that refused to open a damaged bundle would refuse exactly the
/// bundles somebody most wants to look at.
fn read_journal(bytes: &[u8]) -> Journal {
    let records = bytes
        .chunks_exact(DRAW_RECORD_BYTES)
        .filter_map(|chunk| {
            let fixed: &[u8; DRAW_RECORD_BYTES] = chunk.try_into().expect("chunks_exact is exact");
            // A record with rubbish in its reserved field is damage of the same kind as a short
            // tail, and it is skipped for the same reason: the page still shows the run.
            DrawRecord::from_bytes(fixed).ok()
        })
        .collect();
    Journal::from_records(records)
}

/// Parses the record section, one line at a time, keeping what parses.
fn read_records(bytes: &[u8]) -> Vec<Record> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| Record::parse_line(line).ok())
        .collect()
}

/// Ties the first failed assertion on the timeline to the record that says where it is written.
///
/// Matched by name rather than by position: the record section holds every record the run wrote,
/// including from guests that never failed anything, and a positional match would attach the wrong
/// source location to the right assertion — which is worse than attaching none.
fn find_violation(events: &[Event], records: &[Record]) -> Option<Violation> {
    let (event, name, kind) = events.iter().find_map(|e| match &e.kind {
        EventKind::Assertion { name, kind, condition: false, .. } => Some((e, name, *kind)),
        _ => None,
    })?;

    let location = records
        .iter()
        .filter_map(|r| match r {
            Record::Assert(assertion, _) => Some(assertion),
            _ => None,
        })
        .find(|assertion| assertion.message == *name && !assertion.condition)
        .map(|assertion| Location {
            file: assertion.location.file.clone(),
            function: assertion.location.function.clone(),
            line: assertion.location.begin_line,
        });

    Some(Violation { event: event.id, node: event.node, name: name.clone(), kind, location })
}
