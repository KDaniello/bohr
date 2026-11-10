//! What one run came to, as a file a program reads: `bohr run --facts FILE`.
//!
//! A check over many seeds needs the outcome, codes, digest and a few numbers in a stable shape,
//! without decoding a bundle or parsing text meant for people.
//!
//! No events are included; the bundle has them.

use serde::{Deserialize, Serialize};

/// One run's facts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    /// The kind of outcome, as the outcome's own name: `Clean`, `GuestCrashed`, `ProbeViolated`.
    pub outcome: String,
    /// What the outcome says beyond its kind.
    pub detail: String,
    /// Every diagnostic code the outcome names, in the order it names them.
    pub codes: Vec<String>,
    /// The run's identity.
    pub digest: String,
    /// Events the run recorded.
    pub events: u64,
    /// System calls the guests made.
    pub syscalls: u64,
    /// Times the guest crossed into the supervisor: what the run cost.
    pub stops: u64,
    /// Virtual time reached, in nanoseconds.
    pub virtual_ns: u64,
    /// Faults that reached the guest.
    pub adversity: u32,
    /// Requests a probe made once each was first answered.
    pub probe_asked: u64,
    /// How many of those a node answered.
    pub probe_answered: u64,
    /// Bytes of event log the run wrote.
    pub log_bytes: u64,
    /// The supervisor's own peak resident memory, in KiB: the world, the log, the snapshots.
    pub peak_rss_kib: u64,
    /// Processor time the supervisor spent in its own code, in milliseconds.
    pub cpu_user_ms: u64,
    /// Processor time the host kernel spent for it, in milliseconds.
    pub cpu_system_ms: u64,
    /// Processor time its guests spent, in milliseconds.
    pub guest_cpu_ms: u64,
    /// Why the run ended: the horizon, the guest finishing, a node named to end it, a deadlock.
    pub stop: String,
    /// Why determinism was not fully guaranteed; empty when it was.
    pub degraded: Vec<String>,
}

impl Facts {
    /// Whether anything was found.
    #[must_use]
    pub fn found(&self) -> bool {
        self.outcome != "Clean"
    }

    /// Whether the run found what `code` names: an outcome kind, or a code the outcome carries.
    #[must_use]
    pub fn names(&self, code: &str) -> bool {
        self.outcome == code || self.codes.iter().any(|c| c == code)
    }

    /// The facts as the line written to a file.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// How many requests the probes made once each was first answered, and how many of those came back
/// with a result.
///
/// Counted from each request's first answer, so a node's start-up is not counted as failure (reth
/// answers from its tenth virtual second). A request never answered counts nothing: 0 of 0.
#[must_use]
pub fn probe_answers(records: &[u8]) -> (u64, u64) {
    use bohr_sdk_proto::probe::{Answer, PROBE_EVENT, Round};
    let (mut asked, mut answered) = (0, 0);
    let mut started: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for line in records.split(|b| *b == b'\n') {
        let Ok(text) = std::str::from_utf8(line) else { continue };
        let Ok(bohr_sdk_proto::Record::Event(name, details, _)) =
            bohr_sdk_proto::Record::parse_line(text)
        else {
            continue;
        };
        if name != PROBE_EVENT {
            continue;
        }
        let Some(round) = Round::from_details(&details) else { continue };
        for (request, answer) in round.0 {
            let result = matches!(answer, Answer::Result(_));
            if result {
                started.insert(request);
            }
            if started.contains(&request) {
                asked += 1;
                answered += u64::from(result);
            }
        }
    }
    (asked, answered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outcome_splits_into_its_kind_and_its_detail() {
        let outcome =
            bohr_trace::Outcome::ProbeViolated(r#"BOHR-2043: node 1 "signed" twice"#.into());
        assert_eq!(outcome.kind().name(), "ProbeViolated");
        assert_eq!(outcome.code(), Some(bohr_types::Code(2043)));
        let detail = outcome.detail();
        assert_eq!(bohr_trace::Outcome::Clean.detail(), "");
        assert_eq!(bohr_types::Code::all_in(detail), [bohr_types::Code(2043)]);
        assert_eq!(
            bohr_types::Code::all_in("BOHR-2040: a; BOHR-204; BOHR-2041 and BOHR-2040"),
            [bohr_types::Code(2040), bohr_types::Code(2041)]
        );
    }

    #[test]
    fn a_finding_is_named_by_its_kind_or_its_code() {
        let facts = Facts {
            outcome: "ProbeViolated".into(),
            codes: vec!["BOHR-2043".into()],
            ..Facts::default()
        };
        assert!(facts.found());
        assert!(facts.names("BOHR-2043"));
        assert!(facts.names("ProbeViolated"));
        assert!(!facts.names("BOHR-2040"));
    }

    #[test]
    fn probe_rounds_count_what_was_asked_and_answered() {
        let records = br#"{"antithesis_event":{"bohr_probe":{"round":[{"request":0,"unanswered":"refused"},{"request":1,"unanswered":"refused"}]}}}
{"antithesis_event":{"bohr_probe":{"round":[{"request":0,"result":{"n":1}},{"request":1,"unanswered":"refused"}]}}}
{"antithesis_event":{"bohr_probe":{"round":[{"request":0,"unanswered":"timeout"},{"request":1,"unanswered":"refused"}]}}}
{"antithesis_event":{"other":{"round":[{"request":0,"result":1}]}}}
"#;
        // Request 0: refused while starting (not counted), answered, then a timeout: 1 of 2.
        // Request 1: never answered, so never counted.
        assert_eq!(probe_answers(records), (2, 1));
    }
}
