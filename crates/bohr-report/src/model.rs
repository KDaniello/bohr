//! A stored bundle, digested once into the shape every renderer asks it for.
//!
//! The joins every output needs (assertion to source record, draw to event, event to node) are done
//! once here, so outputs agree.
//!
//! Missing sections are normal: a guest without the SDK has no records, and one without coverage
//! has no map.

use std::collections::BTreeMap;

use bohr_sdk_proto::{AssertRecord, Record};
use bohr_tape::{DRAW_RECORD_BYTES, DrawRecord, Journal};
use bohr_trace::{
    BinaryRecord, Event, EventKind, HostProfile, Manifest, SectionId, read_events_lossy,
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
    /// Unknown lines survive as [`Record::Unknown`], so newer bundles still read.
    pub records: Vec<Record>,
    /// What the guest was shown of the machine, where the bundle carries it.
    pub host: Option<HostProfile>,
    /// The objects the run started with.
    pub binaries: Vec<BinaryRecord>,
    /// The effective configuration, verbatim.
    pub config: String,
    /// Whatever anybody wrote on the bundle afterwards.
    pub notes: Vec<String>,
    /// The end of what each incarnation of each node printed, where the bundle kept it.
    pub output: Vec<bohr_trace::OutputTail>,
    /// Where a promise broke, and which promise.
    pub violation: Option<Violation>,
    /// How far the log could be read before it stopped making sense, where it did.
    ///
    /// A truncated log is shown up to the truncation, and the report says so.
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
    /// Where it is written, from the guest's record; absent without the SDK.
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
    /// Only when the manifest or the event log is unreadable.
    pub fn of(bundle: &bohr_trace::Bundle) -> Result<Self, ReportError> {
        let manifest = bundle.manifest().map_err(ReportError::Unreadable)?;
        // A report describes one execution; a bundle of several is refused.
        let single = bundle.single().map_err(ReportError::Unreadable)?;

        let (events, damage) = match single.events().map_err(ReportError::Unreadable)? {
            Some(bytes) => read_events_lossy(bytes),
            None => (Vec::new(), None),
        };
        let journal =
            bundle.section(SectionId::DrawJournal).map_or_else(Journal::new, read_journal);
        let records = single.sdk_records().map(read_records).unwrap_or_default();
        // An unreadable section is read as none: the text is for a reader, the run is elsewhere.
        let host = bundle.host_profile().ok().flatten();
        let binaries = bundle.binaries().unwrap_or_default();
        let config = bundle.config_text().unwrap_or_default().to_owned();
        let notes = bundle
            .section(SectionId::Notes)
            .map(|b| String::from_utf8_lossy(b).lines().map(str::to_owned).collect())
            .unwrap_or_default();
        let output = bundle.output_tails().unwrap_or_default();

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
            output,
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
    /// Each record carries its event, so this is a lookup.
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
/// Fixed-width records, so a short tail is damage; a damaged bundle still opens.
fn read_journal(bytes: &[u8]) -> Journal {
    let (whole, _short_tail) = bytes.as_chunks::<DRAW_RECORD_BYTES>();
    let records = whole
        .iter()
        // A record with a bad reserved field is skipped as damage.
        .filter_map(|fixed| DrawRecord::from_bytes(fixed).ok())
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
/// Matched by name, since the record section holds every guest's records.
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
