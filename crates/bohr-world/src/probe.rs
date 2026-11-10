//! Asking a node's own API what it did, and judging the answers.
//!
//! A blockchain client has no SDK and keeps running when wrong, so it is judged by its own API. The
//! probe guest asks; this module judges its records. See `docs/ARCHITECTURE.md` §5.17.
//!
//! This states a fact and gives it a code; an oracle decides whether it is a finding.

use std::collections::{BTreeMap, BTreeSet};

use bohr_sdk_proto::json::Json;
use bohr_types::Tick;

/// The name of the SDK event the probe reports a round of answers under.
pub const PROBE_EVENT: &str = "bohr_probe";

pub use bohr_trace::{ADVANCES_CODE, AGREES_CODE, MONOTONIC_CODE, NEVER_REVERTS_CODE, UNIQUE_CODE};
/// A probe holds as many keys as it may and compares no new ones.
pub const KEYS_FULL_CODE: &str = "BOHR-2045";

/// Keys one probe holds before it stops taking new ones.
///
/// The node chooses keys, so they are bounded; 10 000 blocks fit easily.
pub const MOST_KEYS: usize = 1 << 16;

/// What a probe holds the answers to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Property {
    /// A value at a key never changes once seen, on one node.
    NeverReverts,
    /// A number at a key never decreases, on one node.
    Monotonic,
    /// Every node answers the same value for the same key.
    Agrees,
    /// The value changes within the span, measured from when the cluster was last whole.
    Advances,
    /// A key carries one value across every node and the whole run.
    Unique,
}

impl Property {
    /// The name a scenario writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NeverReverts => "never-reverts",
            Self::Monotonic => "monotonic",
            Self::Agrees => "agrees",
            Self::Advances => "advances",
            Self::Unique => "unique",
        }
    }

    /// The property of that name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "never-reverts" => Self::NeverReverts,
            "monotonic" => Self::Monotonic,
            "agrees" => Self::Agrees,
            "advances" => Self::Advances,
            "unique" => Self::Unique,
            _ => return None,
        })
    }

    /// The diagnostic a broken property raises.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NeverReverts => NEVER_REVERTS_CODE,
            Self::Agrees => AGREES_CODE,
            Self::Advances => ADVANCES_CODE,
            Self::Unique => UNIQUE_CODE,
            Self::Monotonic => MONOTONIC_CODE,
        }
    }
}

/// One question asked of some nodes on a schedule, and the property its answers must keep.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProbeSpec {
    /// What findings call it.
    pub name: String,
    /// The nodes it asks, by number.
    pub nodes: Vec<u32>,
    /// The port each node answers JSON-RPC on.
    pub port: u16,
    /// The method called.
    pub rpc: String,
    /// The parameters, as the JSON array text the scenario wrote.
    pub params: String,
    /// Virtual time between two askings.
    pub every: Tick,
    /// Fields of the result that together name what a value belongs to; empty where the result
    /// is one value with no key.
    pub key: Vec<String>,
    /// The field of the result that is the value.
    pub value: String,
    /// What the answers must keep.
    pub property: Property,
    /// How long a value may stay the same, for [`Property::Advances`].
    pub within: Option<Tick>,
}

/// One call the probe makes, shared by every probe that asks it of the same node.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    /// The node asked.
    pub node: u32,
    /// Its JSON-RPC port.
    pub port: u16,
    /// The method.
    pub rpc: String,
    /// The parameters, as JSON array text.
    pub params: String,
    /// The fields of the result the probe reports back, and no others.
    ///
    /// Only these, since records are stored verbatim and blocks are large.
    pub fields: Vec<String>,
    /// The shortest interval any probe sharing it asked for.
    pub every: Tick,
}

/// The calls a set of probes amounts to, one per distinct question per node.
///
/// Derived identically by the tool and the world, so request indices agree.
#[must_use]
pub fn requests_of(probes: &[ProbeSpec]) -> Vec<Request> {
    let mut requests: Vec<Request> = Vec::new();
    for probe in probes {
        for &node in &probe.nodes {
            let existing = requests.iter().position(|r| {
                r.node == node
                    && r.port == probe.port
                    && r.rpc == probe.rpc
                    && r.params == probe.params
            });
            let at = existing.unwrap_or_else(|| {
                requests.push(Request {
                    node,
                    port: probe.port,
                    rpc: probe.rpc.clone(),
                    params: probe.params.clone(),
                    fields: Vec::new(),
                    every: probe.every,
                });
                requests.len() - 1
            });
            let request = &mut requests[at];
            request.every = request.every.min(probe.every);
            for field in probe.key.iter().chain(std::iter::once(&probe.value)) {
                if !request.fields.contains(field) {
                    request.fields.push(field.clone());
                }
            }
        }
    }
    requests
}

/// The plan handed to the probe, as one JSON document.
///
/// Params are spliced in as the text the scenario wrote, already checked to be a JSON array.
#[must_use]
pub fn plan_of(requests: &[Request]) -> String {
    let mut out = String::from("{\"requests\":[");
    for (i, r) in requests.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let fields = Json::Array(r.fields.iter().map(|f| Json::Str(f.clone())).collect()).to_json();
        out.push_str(&format!(
            "{{\"node\":{},\"port\":{},\"method\":{},\"params\":{},\"fields\":{},\"every\":{}}}",
            r.node,
            r.port,
            Json::Str(r.rpc.clone()).to_json(),
            r.params,
            fields,
            r.every.get()
        ));
    }
    out.push_str("]}");
    out
}

/// What one request came back with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    /// The result, reduced to the requested fields.
    Result(Json),
    /// Why there is no result: refused, timed out, unreadable, or an error the node returned.
    Unanswered(String),
}

/// Reads one round the probe reported, as `(request, answer)` pairs.
///
/// A malformed round is ignored; answers for unknown requests are dropped.
#[must_use]
pub fn answers_in(details: &Json, requests: usize) -> Vec<(usize, Answer)> {
    let Some(Json::Array(round)) = details.get("round") else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(round.len());
    for item in round {
        let Some(index) = item.get("request").and_then(Json::as_int) else { continue };
        let Ok(index) = usize::try_from(index) else { continue };
        if index >= requests {
            continue;
        }
        let answer = match (item.get("result"), item.get("unanswered")) {
            (Some(result), _) => Answer::Result(result.clone()),
            (None, Some(why)) => Answer::Unanswered(why.as_str().unwrap_or("unanswered").into()),
            (None, None) => continue,
        };
        out.push((index, answer));
    }
    out
}

/// A fact one probe's answers established, with its code.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Broken {
    /// The diagnostic code.
    pub code: &'static str,
    /// The node the answer came from.
    pub node: u32,
    /// What was seen, in words that name the probe, the key and both values.
    pub message: String,
}

/// What one probe has learned so far.
#[derive(Debug, Default)]
struct Learned {
    /// Per node and key, the value seen, for the properties judged on one node.
    by_node: BTreeMap<(u32, Box<str>), Box<str>>,
    /// Per key, the first value seen and the node that answered it, for `unique`.
    by_key: BTreeMap<Box<str>, (Box<str>, u32)>,
    /// Per key, each node's latest value, for `agrees`.
    latest: BTreeMap<Box<str>, BTreeMap<u32, Box<str>>>,
    /// Per node, the last value, when it last changed, and when the node was first asked.
    progress: BTreeMap<u32, (Option<Box<str>>, Tick, Tick)>,
    /// Whether the key bound was reached and said so.
    full: bool,
}

/// Holds every probe's answers and says when one breaks its property.
#[derive(Debug)]
pub struct Observer {
    probes: Vec<ProbeSpec>,
    /// For each request, the probes that read it.
    readers: Vec<Vec<usize>>,
    requests: Vec<Request>,
    learned: Vec<Learned>,
    /// Nodes the world is holding down.
    down: BTreeSet<u32>,
    /// When the cluster last became whole: the last heal, or the last node coming back.
    whole_since: Tick,
    /// The earliest moment a stall is timed from: where the scenario closes its fault window,
    /// `faults.disarm_after_ticks`, a stall is judged only once nothing pushes the cluster down.
    judged_from: Tick,
    /// Probe and node pairs already reported, so a run says each once.
    reported: BTreeSet<(usize, u32)>,
}

impl Observer {
    /// An observer of these probes.
    #[must_use]
    pub fn new(probes: Vec<ProbeSpec>) -> Self {
        let requests = requests_of(&probes);
        let mut readers = vec![Vec::new(); requests.len()];
        for (p, probe) in probes.iter().enumerate() {
            for (r, request) in requests.iter().enumerate() {
                if probe.nodes.contains(&request.node)
                    && request.port == probe.port
                    && request.rpc == probe.rpc
                    && request.params == probe.params
                {
                    readers[r].push(p);
                }
            }
        }
        let learned = probes.iter().map(|_| Learned::default()).collect();
        Self {
            probes,
            readers,
            requests,
            learned,
            down: BTreeSet::new(),
            whole_since: Tick(0),
            judged_from: Tick(0),
            reported: BTreeSet::new(),
        }
    }

    /// Times no stall from before `at`, the moment the scenario's fault window closes.
    pub const fn judge_stalls_from(&mut self, at: Tick) {
        self.judged_from = at;
    }

    /// How many requests the plan has.
    #[must_use]
    pub fn requests(&self) -> usize {
        self.requests.len()
    }

    /// A node the world took away, which is silent by our doing.
    ///
    /// Its progress is reset, so its boot is not a stall.
    pub fn node_down(&mut self, node: u32) {
        self.down.insert(node);
        for learned in &mut self.learned {
            learned.progress.remove(&node);
        }
    }

    /// A node the world gave back; the cluster counts as whole from the latest such moment.
    pub fn node_up(&mut self, node: u32, now: Tick) {
        self.down.remove(&node);
        self.whole_since = self.whole_since.max(now);
    }

    /// A node the world rolled back, by a reorg or a restart from an older snapshot.
    ///
    /// Its earlier answers are forgotten, so the rollback is not its revert.
    pub fn rewound(&mut self, node: u32) {
        for learned in &mut self.learned {
            learned.by_node.retain(|(n, _), _| *n != node);
            for answers in learned.latest.values_mut() {
                answers.remove(&node);
            }
        }
    }

    /// The last cut lifted.
    pub fn healed(&mut self, now: Tick) {
        self.whole_since = self.whole_since.max(now);
    }

    /// Takes one answer and says what it broke.
    ///
    /// `cut` says whether a partition is in force, which excuses a stall.
    pub fn answer(&mut self, now: Tick, cut: bool, request: usize, answer: &Answer) -> Vec<Broken> {
        let Some(node) = self.requests.get(request).map(|r| r.node) else {
            return Vec::new();
        };
        let mut broken = Vec::new();
        for p in self.readers[request].clone() {
            let probe = &self.probes[p];
            let property = probe.property;
            let seen = match answer {
                Answer::Result(result) => observations(result, &probe.key, &probe.value),
                Answer::Unanswered(_) => Vec::new(),
            };
            let found = match property {
                Property::Advances => {
                    let whole = !cut && self.down.is_empty();
                    self.advances(p, node, now, whole, seen.first().map(|(_, v)| v.as_str()))
                }
                _ => self.keyed(p, node, &seen),
            };
            for message in found {
                // Reported once; held keys are still compared.
                if message.0 == KEYS_FULL_CODE || self.reported.insert((p, node)) {
                    broken.push(Broken { code: message.0, node, message: message.1 });
                }
            }
        }
        broken
    }

    /// Judges the properties that compare values at a key.
    fn keyed(
        &mut self,
        p: usize,
        node: u32,
        seen: &[(String, String)],
    ) -> Vec<(&'static str, String)> {
        let probe = &self.probes[p];
        let learned = &mut self.learned[p];
        let mut out = Vec::new();
        for (key, value) in seen {
            let (size, held) = match probe.property {
                Property::NeverReverts | Property::Monotonic => (
                    learned.by_node.len(),
                    learned.by_node.contains_key(&(node, key.as_str().into())),
                ),
                Property::Agrees => {
                    (learned.latest.len(), learned.latest.contains_key(key.as_str()))
                }
                _ => (learned.by_key.len(), learned.by_key.contains_key(key.as_str())),
            };
            if !held && size >= MOST_KEYS {
                if !learned.full {
                    learned.full = true;
                    out.push((
                        KEYS_FULL_CODE,
                        format!(
                            "probe {:?} holds {MOST_KEYS} keys and compares no new ones",
                            probe.name
                        ),
                    ));
                }
                continue;
            }
            match probe.property {
                Property::NeverReverts | Property::Monotonic => {
                    let slot = learned.by_node.entry((node, key.as_str().into()));
                    match slot {
                        std::collections::btree_map::Entry::Vacant(v) => {
                            v.insert(value.as_str().into());
                        }
                        std::collections::btree_map::Entry::Occupied(mut o) => {
                            let was = o.get().to_string();
                            if probe.property == Property::NeverReverts && was != *value {
                                out.push((
                                    NEVER_REVERTS_CODE,
                                    format!(
                                        "probe {:?}: node {node} answered {} at {}, and later {}",
                                        probe.name,
                                        was,
                                        shown(key),
                                        value
                                    ),
                                ));
                            }
                            if probe.property == Property::Monotonic {
                                match (number(&was), number(value)) {
                                    (Some(a), Some(b)) if b < a => out.push((
                                        MONOTONIC_CODE,
                                        format!(
                                            "probe {:?}: node {node} answered {} at {}, and \
                                             later {}",
                                            probe.name,
                                            was,
                                            shown(key),
                                            value
                                        ),
                                    )),
                                    _ => {}
                                }
                                o.insert(value.as_str().into());
                            }
                        }
                    }
                }
                // Latest answers compared across nodes.
                Property::Agrees => {
                    let answers = learned.latest.entry(key.as_str().into()).or_default();
                    let other = answers
                        .iter()
                        .find(|(n, v)| **n != node && v.as_ref() != value.as_str())
                        .map(|(n, v)| (*n, v.to_string()));
                    answers.insert(node, value.as_str().into());
                    if let Some((first, was)) = other {
                        out.push((
                            AGREES_CODE,
                            format!(
                                "probe {:?}: node {first} answered {was} at {}, and node {node} \
                                 answered {value}",
                                probe.name,
                                shown(key)
                            ),
                        ));
                    }
                }
                Property::Unique => match learned.by_key.get(key.as_str()) {
                    None => {
                        learned.by_key.insert(key.as_str().into(), (value.as_str().into(), node));
                    }
                    Some((was, first)) if was.as_ref() != value.as_str() => {
                        out.push((
                            UNIQUE_CODE,
                            format!(
                                "probe {:?}: node {first} answered {was} at {}, and node {node} \
                                 answered {value}",
                                probe.name,
                                shown(key)
                            ),
                        ));
                    }
                    Some(_) => {}
                },
                Property::Advances => {}
            }
        }
        out
    }

    /// Judges whether a node's value moved within the span.
    ///
    /// Timed from the latest of the value's change, the node's first answer since coming up, and
    /// the cluster becoming whole; judged only when nothing is down or cut.
    fn advances(
        &mut self,
        p: usize,
        node: u32,
        now: Tick,
        whole: bool,
        value: Option<&str>,
    ) -> Vec<(&'static str, String)> {
        let probe = &self.probes[p];
        let Some(within) = probe.within else { return Vec::new() };
        let progress = &mut self.learned[p].progress;
        if value.is_none() && !progress.contains_key(&node) {
            return Vec::new();
        }
        let (last, changed, first) = progress.entry(node).or_insert((None, now, now));
        if let Some(value) = value
            && last.as_deref() != Some(value)
        {
            *last = Some(value.into());
            *changed = now;
        }
        if !whole {
            return Vec::new();
        }
        let from = (*changed).max(*first).max(self.whole_since).max(self.judged_from);
        let still = now.get().saturating_sub(from.get());
        if still < within.get() {
            return Vec::new();
        }
        let at = last.as_deref().unwrap_or("its first answer");
        vec![(
            ADVANCES_CODE,
            format!(
                "probe {:?}: node {node} stayed at {at} for {still} ticks, the cluster whole since \
                 tick {}",
                probe.name,
                self.whole_since.get()
            ),
        )]
    }
}

/// A key as a reader would say it.
fn shown(key: &str) -> String {
    if key.is_empty() { "its only key".to_owned() } else { format!("key {key}") }
}

/// The `(key, value)` pairs one result holds: one for an object, one per element for an array,
/// none for `null` or for an element missing the value or any key field.
fn observations(result: &Json, key: &[String], value: &str) -> Vec<(String, String)> {
    let one = |item: &Json| -> Option<(String, String)> {
        let v = field(item, value)?;
        if *v == Json::Null {
            return None;
        }
        // A missing key field means no key.
        let k: Vec<String> = key.iter().map(|k| field(item, k).map(text)).collect::<Option<_>>()?;
        Some((k.join("/"), text(v)))
    };
    match result {
        Json::Array(items) => items.iter().filter_map(one).collect(),
        Json::Null => Vec::new(),
        item => one(item).into_iter().collect(),
    }
}

/// The value at a dotted path; `.` is the whole item.
fn field<'a>(item: &'a Json, path: &str) -> Option<&'a Json> {
    if path == "." {
        return Some(item);
    }
    path.split('.').try_fold(item, |at, part| at.get(part))
}

/// A value as text: a string as itself, anything else as JSON.
fn text(value: &Json) -> String {
    match value {
        Json::Str(s) => s.clone(),
        other => other.to_json(),
    }
}

/// A value as a number: decimal, or hexadecimal with `0x`, as JSON-RPC writes quantities.
fn number(text: &str) -> Option<u128> {
    match text.strip_prefix("0x") {
        Some(hex) => u128::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(property: Property) -> ProbeSpec {
        ProbeSpec {
            name: "finality".to_owned(),
            nodes: vec![0, 1],
            port: 8545,
            rpc: "eth_getBlockByNumber".to_owned(),
            params: "[\"finalized\",false]".to_owned(),
            every: Tick(10),
            key: if property == Property::Advances { vec![] } else { vec!["number".to_owned()] },
            value: if property == Property::Advances { "number" } else { "hash" }.to_owned(),
            property,
            within: (property == Property::Advances).then_some(Tick(100)),
        }
    }

    fn block(number: &str, hash: &str) -> Answer {
        let mut map = BTreeMap::new();
        map.insert("number".to_owned(), Json::Str(number.to_owned()));
        map.insert("hash".to_owned(), Json::Str(hash.to_owned()));
        Answer::Result(Json::Object(map))
    }

    #[test]
    fn probes_asking_the_same_question_share_one_request() {
        let probes = vec![spec(Property::NeverReverts), spec(Property::Agrees)];
        let requests = requests_of(&probes);
        assert_eq!(requests.len(), 2, "one per node, not one per probe");
        assert_eq!(requests[0].fields, ["number", "hash"]);
        let plan = plan_of(&requests);
        assert!(bohr_sdk_proto::json::parse(&plan).is_ok(), "the plan is JSON: {plan}");
    }

    #[test]
    fn a_finalised_hash_that_changes_is_a_revert() {
        let mut o = Observer::new(vec![spec(Property::NeverReverts)]);
        assert!(o.answer(Tick(1), false, 0, &block("0x5", "0xaa")).is_empty());
        assert!(o.answer(Tick(2), false, 0, &block("0x6", "0xbb")).is_empty());
        assert!(o.answer(Tick(3), false, 1, &block("0x5", "0xcc")).is_empty(), "another node");
        let broken = o.answer(Tick(4), false, 0, &block("0x5", "0xdd"));
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].code, NEVER_REVERTS_CODE);
        assert!(broken[0].message.contains("0xaa") && broken[0].message.contains("0xdd"));
        assert!(o.answer(Tick(5), false, 0, &block("0x5", "0xee")).is_empty(), "said once");
    }

    #[test]
    fn two_nodes_with_two_hashes_at_one_height_disagree() {
        let mut o = Observer::new(vec![spec(Property::Agrees)]);
        assert!(o.answer(Tick(1), false, 0, &block("0x5", "0xaa")).is_empty());
        assert!(o.answer(Tick(2), false, 0, &block("0x5", "0xbb")).is_empty(), "one node");
        let broken = o.answer(Tick(3), false, 1, &block("0x5", "0xcc"));
        assert_eq!(broken[0].code, AGREES_CODE);
    }

    #[test]
    fn a_signer_with_two_votes_at_one_height_is_caught_on_any_node() {
        let mut probe = spec(Property::Unique);
        probe.key = vec!["signer".to_owned(), "height".to_owned()];
        let mut o = Observer::new(vec![probe]);
        let vote = |signer: &str, height: &str, hash: &str| {
            let mut map = BTreeMap::new();
            map.insert("signer".to_owned(), Json::Str(signer.to_owned()));
            map.insert("height".to_owned(), Json::Str(height.to_owned()));
            map.insert("hash".to_owned(), Json::Str(hash.to_owned()));
            Json::Object(map)
        };
        let votes =
            Answer::Result(Json::Array(vec![vote("0", "0x5", "0xaa"), vote("1", "0x5", "0xaa")]));
        assert!(o.answer(Tick(1), false, 0, &votes).is_empty());
        let twice = Answer::Result(Json::Array(vec![vote("0", "0x5", "0xbb")]));
        let broken = o.answer(Tick(2), false, 0, &twice);
        assert_eq!(broken[0].code, UNIQUE_CODE);
        assert!(broken[0].message.contains("0/0x5"), "{}", broken[0].message);
    }

    #[test]
    fn a_height_that_stays_while_the_cluster_is_whole_is_a_stall() {
        let mut o = Observer::new(vec![spec(Property::Advances)]);
        assert!(o.answer(Tick(0), false, 0, &block("0x1", "0xa")).is_empty());
        assert!(o.answer(Tick(50), false, 0, &block("0x2", "0xb")).is_empty());
        assert!(o.answer(Tick(140), false, 0, &block("0x2", "0xb")).is_empty(), "90 < 100");
        assert!(o.answer(Tick(200), true, 0, &block("0x2", "0xb")).is_empty(), "cut");
        o.node_down(1);
        assert!(o.answer(Tick(210), false, 0, &block("0x2", "0xb")).is_empty(), "a node down");
        o.node_up(1, Tick(220));
        assert!(o.answer(Tick(300), false, 0, &block("0x2", "0xb")).is_empty(), "whole for 80");
        let broken = o.answer(Tick(330), false, 0, &Answer::Unanswered("refused".into()));
        assert_eq!(broken[0].code, ADVANCES_CODE);
    }

    /// Inside the fault window a stall is the faults'; it is timed from the window's close.
    #[test]
    fn a_stall_is_timed_from_the_close_of_the_fault_window() {
        let mut o = Observer::new(vec![spec(Property::Advances)]);
        o.judge_stalls_from(Tick(500));
        assert!(o.answer(Tick(0), false, 0, &block("0x1", "0xa")).is_empty());
        assert!(o.answer(Tick(450), false, 0, &block("0x1", "0xa")).is_empty(), "inside");
        assert!(o.answer(Tick(590), false, 0, &block("0x1", "0xa")).is_empty(), "90 after");
        let broken = o.answer(Tick(600), false, 0, &block("0x1", "0xa"));
        assert_eq!(broken[0].code, ADVANCES_CODE, "100 after the close");
    }

    /// A node is compared by its latest answer.
    #[test]
    fn agreement_is_with_what_the_others_say_now() {
        let mut o = Observer::new(vec![spec(Property::Agrees)]);
        assert!(o.answer(Tick(1), false, 0, &block("0x5", "0xaa")).is_empty());
        assert!(o.answer(Tick(2), false, 0, &block("0x5", "0xbb")).is_empty(), "a revert");
        assert!(o.answer(Tick(3), false, 1, &block("0x5", "0xbb")).is_empty(), "agrees now");
    }

    /// An answer without a key field has no key.
    #[test]
    fn an_answer_missing_a_key_field_is_no_observation() {
        let mut probe = spec(Property::Unique);
        probe.key = vec!["signer".to_owned(), "height".to_owned()];
        let mut o = Observer::new(vec![probe]);
        assert!(o.answer(Tick(1), false, 0, &block("0x5", "0xaa")).is_empty());
        assert!(o.answer(Tick(2), false, 0, &block("0x6", "0xbb")).is_empty());
    }

    /// A restarted node is timed from its first answer.
    #[test]
    fn a_node_is_not_stalled_while_it_boots() {
        let mut o = Observer::new(vec![spec(Property::Advances)]);
        assert!(o.answer(Tick(0), false, 0, &block("0x1", "0xa")).is_empty());
        o.node_down(0);
        o.node_up(0, Tick(10));
        for t in [50, 150, 250] {
            let refused = Answer::Unanswered("connect: connection refused".into());
            assert!(o.answer(Tick(t), false, 0, &refused).is_empty(), "booting at {t}");
        }
        assert!(o.answer(Tick(300), false, 0, &block("0x1", "0xa")).is_empty(), "first answer");
        assert_eq!(o.answer(Tick(401), false, 0, &block("0x1", "0xa"))[0].code, ADVANCES_CODE);
    }

    /// A world rollback is not the node's revert.
    #[test]
    fn a_rewind_by_the_world_is_not_a_revert() {
        let mut o = Observer::new(vec![spec(Property::NeverReverts)]);
        assert!(o.answer(Tick(1), false, 0, &block("0x5", "0xaa")).is_empty());
        o.rewound(0);
        assert!(o.answer(Tick(2), false, 0, &block("0x5", "0xbb")).is_empty());
    }

    #[test]
    fn no_value_is_no_observation() {
        let mut o = Observer::new(vec![spec(Property::NeverReverts)]);
        assert!(o.answer(Tick(1), false, 0, &Answer::Result(Json::Null)).is_empty());
        assert!(o.answer(Tick(2), false, 0, &block("0x5", "0xaa")).is_empty());
        assert!(o.answer(Tick(3), false, 0, &Answer::Unanswered("timeout".into())).is_empty());
    }

    #[test]
    fn a_number_that_goes_down_is_caught() {
        let mut probe = spec(Property::Monotonic);
        probe.key = vec![];
        probe.value = "number".to_owned();
        let mut o = Observer::new(vec![probe]);
        assert!(o.answer(Tick(1), false, 0, &block("0x9", "0xa")).is_empty());
        assert!(o.answer(Tick(2), false, 0, &block("0xa", "0xa")).is_empty());
        assert_eq!(o.answer(Tick(3), false, 0, &block("0x3", "0xa"))[0].code, MONOTONIC_CODE);
    }

    #[test]
    fn a_round_is_read_by_request() {
        let details = bohr_sdk_proto::json::parse(
            r#"{"round":[{"request":0,"result":{"number":"0x1"}},{"request":1,"unanswered":"refused"},{"request":9,"result":null}]}"#,
        )
        .expect("json");
        let answers = answers_in(&details, 2);
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[1], (1, Answer::Unanswered("refused".to_owned())));
    }
}
