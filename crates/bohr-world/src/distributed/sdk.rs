//! Records a guest writes about itself, on their way into the trace.
//!
//! An SDK writes lines to a descriptor through ordinary writes. This module recognises the
//! descriptor, splits lines, and turns each into an event and a stored record. See
//! `docs/ARCHITECTURE.md` §7.4.

use bohr_sdk_proto::Record;
use bohr_trace::{AssertionKind, EventKind};
use bohr_types::NodeId;

/// The diagnostic a failed per-run assertion raises.
///
/// The violation is an event, since the oracle judges what the run recorded.
pub(crate) const VIOLATION_CODE: &str = "BOHR-2012";

/// Everything the guests have said about themselves.
#[derive(Debug, Default)]
pub(crate) struct SdkRecords {
    /// Lines as they arrived, verbatim, so fields from newer SDKs survive.
    lines: Vec<String>,
    /// What each node's descriptor has received but not yet terminated with a newline.
    partial: std::collections::BTreeMap<(NodeId, i64), String>,
    /// The last sequence number seen from each node, for noticing a gap.
    last_seq: std::collections::BTreeMap<NodeId, u64>,
    /// Lines that did not parse, counted but not kept.
    malformed: u64,
    /// The node the probe runs on, the one node whose probe rounds are read as answers.
    ///
    /// Only the probe's rounds count, so a node cannot grade itself.
    pub(crate) probe_node: Option<NodeId>,
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
    /// Rounds of answers the probe reported, each as the event's details.
    pub(crate) probe_rounds: Vec<bohr_sdk_proto::json::Json>,
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
    /// Partial lines are held until their newline.
    pub(crate) fn accept(&mut self, node: NodeId, fd: i64, bytes: &[u8]) -> Digested {
        let mut digested = Digested::default();

        // Lines are taken out first, then digested, to avoid a double borrow.
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
                // Dropped, counted, and reported once.
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
            // Per-node sequence numbers start at one; a gap means records escaped, and the run
            // continues degraded.
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

        if let Record::Event(name, details, _) = &record
            && name == crate::probe::PROBE_EVENT
            && self.probe_node == Some(node)
        {
            digested.probe_rounds.push(details.clone());
        }

        if let Record::Assert(assertion, _) = &record {
            let kind = match assertion.assert_type {
                bohr_sdk_proto::AssertType::Always => AssertionKind::Always,
                bohr_sdk_proto::AssertType::AlwaysOrUnreachable => {
                    AssertionKind::AlwaysOrUnreachable
                }
                bohr_sdk_proto::AssertType::Sometimes => AssertionKind::Sometimes,
                bohr_sdk_proto::AssertType::Reachable => AssertionKind::Reachable,
                bohr_sdk_proto::AssertType::Unreachable => AssertionKind::Unreachable,
                // A numeric guard maps to its nearest kind; the record has the rest.
                bohr_sdk_proto::AssertType::NumericGuard(_) => AssertionKind::Sometimes,
            };
            digested.events.push(EventKind::Assertion {
                name: assertion.message.clone(),
                kind,
                hit: assertion.hit,
                condition: assertion.condition,
            });

            // Only per-run kinds fail here.
            if assertion.assert_type.is_per_run() && !assertion.condition {
                let detail = format!(
                    "{}(\"{}\") failed at {}:{}",
                    assertion.assert_type.as_str(),
                    assertion.message,
                    assertion.location.file,
                    assertion.location.begin_line
                );
                // Also a diagnostic, which the oracle reads.
                digested.diagnostics.push((VIOLATION_CODE.to_owned(), detail.clone()));
                digested.violated.get_or_insert(detail);
            }
        }
    }
}
