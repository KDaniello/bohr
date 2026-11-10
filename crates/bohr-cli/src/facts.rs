//! What one run came to, as a file a program reads: `bohr run --facts FILE`.
//!
//! A check over many seeds needs the outcome, codes, digest and a few numbers in a stable shape,
//! without decoding a bundle or parsing text meant for people.
//!
//! No events are included; the bundle has them.

use serde::{Deserialize, Serialize};

use crate::execute::Completed;

/// One run's facts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Facts {
    /// The kind of outcome, as the outcome's own name: `Clean`, `GuestCrashed`, `ProbeViolated`.
    pub(crate) outcome: String,
    /// What the outcome says beyond its kind.
    pub(crate) detail: String,
    /// Every diagnostic code the outcome names, in the order it names them.
    pub(crate) codes: Vec<String>,
    /// The run's identity.
    pub(crate) digest: String,
    pub(crate) events: u64,
    pub(crate) syscalls: u64,
    /// Times the guest crossed into the supervisor: what the run cost.
    pub(crate) stops: u64,
    /// Virtual time reached, in nanoseconds.
    pub(crate) virtual_ns: u64,
    /// Faults that reached the guest.
    pub(crate) adversity: u32,
    /// Requests a probe made once each was first answered, and how many of them a node answered.
    pub(crate) probe_asked: u64,
    pub(crate) probe_answered: u64,
    /// Bytes of event log the run wrote.
    pub(crate) log_bytes: u64,
    /// The supervisor's own peak resident memory, in KiB: the world, the log, the snapshots.
    pub(crate) peak_rss_kib: u64,
    /// Processor time the supervisor spent, in its own code and in the host kernel for it, and
    /// the time its guests spent, in milliseconds.
    pub(crate) cpu_user_ms: u64,
    pub(crate) cpu_system_ms: u64,
    pub(crate) guest_cpu_ms: u64,
    /// Why the run ended: the horizon, the guest finishing, a node named to end it, a deadlock.
    pub(crate) stop: String,
    /// Why determinism was not fully guaranteed; empty when it was.
    pub(crate) degraded: Vec<String>,
}

impl Facts {
    /// The facts of a completed run.
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
    pub(crate) fn of(completed: &Completed) -> Self {
        let outcome = &completed.report.outcome;
        let (outcome, detail) = split(&format!("{outcome:?}"));
        let (probe_asked, probe_answered) = probe_answers(&completed.sdk_records);
        let cpu = own_cpu_ms();
        Self {
            codes: codes_in(&detail),
            outcome,
            detail,
            digest: completed.report.digest.to_string(),
            events: completed.report.events,
            syscalls: completed.stats.syscalls,
            stops: completed.report.metrics.stops,
            virtual_ns: completed.report.ticks.get(),
            adversity: completed.report.adversity,
            probe_asked,
            probe_answered,
            stop: format!("{:?}", completed.report.stop_reason),
            log_bytes: completed.log.len() as u64,
            peak_rss_kib: own_peak_rss_kib(),
            cpu_user_ms: cpu.0,
            cpu_system_ms: cpu.1,
            guest_cpu_ms: cpu.2,
            degraded: completed.degradations.clone(),
        }
    }

    /// Whether anything was found.
    pub(crate) fn found(&self) -> bool {
        self.outcome != "Clean"
    }

    /// Whether the run found what `code` names: an outcome kind, or a code the outcome carries.
    pub(crate) fn names(&self, code: &str) -> bool {
        self.outcome == code || self.codes.iter().any(|c| c == code)
    }

    /// The facts as the line written to a file.
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
    pub(crate) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// The supervisor's peak resident set, read from `/proc/self/status`; 0 where there is none.
///
/// Read after the run, so it covers the whole run at no cost to it.
fn own_peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
            })
        })
        .unwrap_or(0)
}

/// The supervisor's user and system time and its reaped guests' time, in milliseconds, read from
/// `/proc/self/stat`; zeros where there is none.
///
/// The fields count 10 ms ticks, the unit x86-64 Linux reports to user space.
fn own_cpu_ms() -> (u64, u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0, 0);
    };
    // The command name is in parentheses and may hold spaces; the fields that follow it are
    // numbered from 3, so utime, stime, cutime and cstime are the 12th to the 15th after it.
    let after = text.rsplit_once(')').map_or("", |(_, rest)| rest);
    let fields: Vec<u64> =
        after.split_whitespace().skip(11).take(4).map(|f| f.parse().unwrap_or(0)).collect();
    match fields[..] {
        [user, system, child_user, child_system] => {
            (user * 10, system * 10, (child_user + child_system) * 10)
        }
        _ => (0, 0, 0),
    }
}

/// An outcome's debug form, `Kind("detail")`, as its kind and its detail.
///
/// Parsed from the debug form, so a new outcome variant needs no change here.
fn split(debug: &str) -> (String, String) {
    match debug.split_once('(') {
        Some((kind, rest)) => {
            let inner = rest.strip_suffix(')').unwrap_or(rest);
            let detail = serde_json::from_str::<String>(inner).unwrap_or_else(|_| inner.to_owned());
            (kind.to_owned(), detail)
        }
        None => (debug.to_owned(), String::new()),
    }
}

/// Every `BOHR-NNNN` in a text, once each, in order.
fn codes_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while let Some(found) = text[at..].find("BOHR-") {
        let start = at + found;
        let digits = bytes[start + 5..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 4 {
            let code = text[start..start + 9].to_owned();
            if !out.contains(&code) {
                out.push(code);
            }
        }
        at = start + 5;
    }
    out
}

/// How many requests the probes made once each was first answered, and how many of those came back
/// with a result.
///
/// Counted from each request's first answer, so a node's start-up is not counted as failure (reth
/// answers from its tenth virtual second). A request never answered counts nothing: 0 of 0.
fn probe_answers(records: &[u8]) -> (u64, u64) {
    let (mut asked, mut answered) = (0, 0);
    let mut started: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    for line in records.split(|b| *b == b'\n') {
        let Ok(text) = std::str::from_utf8(line) else { continue };
        if !text.contains(bohr_world::probe::PROBE_EVENT) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else { continue };
        let Some(round) = find_round(&value) else { continue };
        for item in round {
            let request = item.get("request").and_then(serde_json::Value::as_i64).unwrap_or(-1);
            let result = item.get("result").is_some();
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

/// The `round` array wherever the record's envelope put it.
fn find_round(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Array(round)) = map.get("round") {
                return Some(round);
            }
            map.values().find_map(find_round)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_round),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outcome_splits_into_its_kind_and_its_detail() {
        assert_eq!(split("Clean"), ("Clean".to_owned(), String::new()));
        let (kind, detail) = split(r#"ProbeViolated("BOHR-2043: node 1 \"signed\" twice")"#);
        assert_eq!(kind, "ProbeViolated");
        assert_eq!(detail, r#"BOHR-2043: node 1 "signed" twice"#);
        assert_eq!(codes_in(&detail), ["BOHR-2043"]);
        assert_eq!(
            codes_in("BOHR-2040: a; BOHR-204; BOHR-2041 and BOHR-2040"),
            ["BOHR-2040", "BOHR-2041"]
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
        let records = br#"{"kind":"event","name":"bohr_probe","details":{"round":[{"request":0,"unanswered":"refused"},{"request":1,"unanswered":"refused"}]}}
{"kind":"event","name":"bohr_probe","details":{"round":[{"request":0,"result":{"n":1}},{"request":1,"unanswered":"refused"}]}}
{"kind":"event","name":"bohr_probe","details":{"round":[{"request":0,"unanswered":"timeout"},{"request":1,"unanswered":"refused"}]}}
{"kind":"event","name":"other","details":{"round":[{"request":0,"result":1}]}}
"#;
        // Request 0: refused while starting (not counted), answered, then a timeout: 1 of 2.
        // Request 1: never answered, so never counted.
        assert_eq!(probe_answers(records), (2, 1));
    }
}
