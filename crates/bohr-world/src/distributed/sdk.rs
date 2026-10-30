//! Records a guest writes about itself, on their way into the trace.
//!
//! An SDK writes lines to a descriptor, and a write is a syscall the world already answers — so
//! there is no channel here beyond the one every guest has. What this module does is recognise
//! the descriptor, split what arrives into lines, and turn each into an event and a stored record.
//! See `docs/ARCHITECTURE.md` §7.4.

use bohr_sdk_proto::Record;
use bohr_trace::{AssertionKind, EventKind};
use bohr_types::NodeId;

/// The diagnostic a failed per-run assertion raises.
///
/// A run is judged from what it recorded, so the violation travels as an event like everything
/// else rather than as a second channel into the oracle.
pub(crate) const VIOLATION_CODE: &str = "BOHR-2012";

/// Everything the guests have said about themselves.
#[derive(Debug, Default)]
pub(crate) struct SdkRecords {
    /// Lines as they arrived, in order. Stored verbatim: a record written by a newer SDK carries
    /// fields this build does not model, and rewriting it would lose exactly those.
    lines: Vec<String>,
    /// What each node's descriptor has received but not yet terminated with a newline.
    partial: std::collections::BTreeMap<(NodeId, i64), String>,
    /// The last sequence number seen from each node, for noticing a gap.
    last_seq: std::collections::BTreeMap<NodeId, u64>,
    /// Lines that did not parse. Counted rather than kept: a malformed line is a defect in
    /// whatever wrote it, and a run that filled its bundle with them would bury the rest.
    malformed: u64,
}

/// What a batch of records asks the run to do.
#[derive(Debug, Default)]
pub(crate) struct Digested {
    /// Events to record, in order.
    pub(crate) events: Vec<EventKind>,
    /// Diagnostics the supervisor noticed while reading them.
    pub(crate) diagnostics: Vec<(String, String)>,
    /// An assertion that failed and ends the run as a finding, where one did.
    pub(crate) violated: Option<String>,
}

impl SdkRecords {
    /// The stored section, one record per line.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.into_bytes()
    }

    /// Takes what a guest wrote to its record descriptor.
    ///
    /// Partial lines are held: a write is not a record, and an SDK that flushed mid-line would
    /// otherwise have half its record read as malformed and the other half as a new one.
    pub(crate) fn accept(&mut self, node: NodeId, fd: i64, bytes: &[u8]) -> Digested {
        let mut digested = Digested::default();

        // Complete lines come out of the buffer first and are digested after: digesting touches
        // the whole record store, and holding the buffer across it would borrow twice.
        let mut complete = Vec::new();
        {
            let buffered = self.partial.entry((node, fd)).or_default();
            buffered.push_str(&String::from_utf8_lossy(bytes));
            while let Some(end) = buffered.find('\n') {
                let line: String = buffered.drain(..=end).collect();
                let line = line.trim_end_matches(['\n', '\r']).to_owned();
                if !line.trim().is_empty() {
                    complete.push(line);
                }
            }
        }
        for line in complete {
            self.digest_line(node, line, &mut digested);
        }
        digested
    }

    fn digest_line(&mut self, node: NodeId, line: String, digested: &mut Digested) {
        let record = match Record::parse_line(&line) {
            Ok(record) => record,
            Err(reason) => {
                // Dropped, counted, and reported once so a run that produces a stream of them
                // still says what happened without the log becoming the stream.
                self.malformed += 1;
                if self.malformed == 1 {
                    digested.diagnostics.push((
                        "BOHR-2021".to_owned(),
                        format!("an SDK record did not parse: {reason}"),
                    ));
                }
                return;
            }
        };

        if let Some(meta) = record.meta() {
            let last = self.last_seq.entry(node).or_insert(0);
            // Sequence numbers are per node and start at one; anything but the next one means
            // records escaped interception, which makes the record stream incomplete rather than
            // wrong. The run continues, degraded and saying so.
            if meta.seq != *last + 1 {
                digested.diagnostics.push((
                    "BOHR-2021".to_owned(),
                    format!("SDK records jumped from {last} to {} on {node}", meta.seq),
                ));
            }
            *last = meta.seq;
        }

        let index = self.lines.len() as u32;
        self.lines.push(line);
        digested.events.push(EventKind::SdkRecord { index });

        if let Record::Assert(assertion, _) = &record {
            let kind = match assertion.assert_type {
                bohr_sdk_proto::AssertType::Always => AssertionKind::Always,
                bohr_sdk_proto::AssertType::AlwaysOrUnreachable => {
                    AssertionKind::AlwaysOrUnreachable
                }
                bohr_sdk_proto::AssertType::Sometimes => AssertionKind::Sometimes,
                bohr_sdk_proto::AssertType::Reachable => AssertionKind::Reachable,
                bohr_sdk_proto::AssertType::Unreachable => AssertionKind::Unreachable,
                // A numeric guard is evaluated over a campaign; on the timeline it is what it
                // most nearly is, and its own record carries the rest.
                bohr_sdk_proto::AssertType::NumericGuard(_) => AssertionKind::Sometimes,
            };
            digested.events.push(EventKind::Assertion {
                name: assertion.message.clone(),
                kind,
                hit: assertion.hit,
                condition: assertion.condition,
            });

            // Only the per-run kinds can fail here. Whether something never happens is a question
            // about a campaign, and one run cannot answer it.
            if assertion.assert_type.is_per_run() && !assertion.condition {
                let detail = format!(
                    "{}(\"{}\") failed at {}:{}",
                    assertion.assert_type.as_str(),
                    assertion.message,
                    assertion.location.file,
                    assertion.location.begin_line
                );
                // Raised as a diagnostic as well, because that is what the oracle reads: a run is
                // judged from what it recorded, never from a side channel into the world.
                digested.diagnostics.push((VIOLATION_CODE.to_owned(), detail.clone()));
                digested.violated.get_or_insert(detail);
            }
        }
    }
}
