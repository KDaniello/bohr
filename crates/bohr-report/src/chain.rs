//! The chain's timeline: what each node answered the probe, over virtual time, beside its faults.
//!
//! One lane per node: virtual time across, the probed key up, the value as a colour. A revert is a
//! dot at a known key in a new colour; a fork is two colours at one key; a stall is a lane that
//! stops climbing. Dead stretches are shaded, and a line marks the violation.
//!
//! Each answer is drawn the first time a node gives it, which keeps the page within its size bound.
//!
//! Lanes come from the caller, who derives them from the scenario.

use std::{collections::BTreeSet, fmt::Write as _};

use bohr_sdk_proto::{Record, json::Json};
use bohr_trace::{EventKind, NodeState};

use crate::{RunModel, escape};

/// What one probe request reads: the node it asks, and the fields of its answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lane {
    /// The node asked.
    pub node: u32,
    /// The field an answer is keyed by, such as a block number; `None` where there is none.
    pub key: Option<String>,
    /// The field compared, such as a hash.
    pub value: String,
}

const WIDTH: u64 = 960;
const LEFT: u64 = 80;
const RIGHT: u64 = 20;
const LANE: u64 = 70;
const GAP: u64 = 14;
const TOP: u64 = 24;

/// One answer as drawn.
struct Dot {
    tick: u64,
    node: u32,
    key: Option<String>,
    value: String,
}

fn text_of(json: &Json) -> String {
    match json {
        Json::Str(s) => s.clone(),
        other => other.to_json(),
    }
}

/// A key as a height: `0x1f` and `31` both, so a lane can be laid out by it.
fn height(key: &str) -> Option<u64> {
    match key.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => key.parse().ok(),
    }
}

/// A colour for a value, the same for the same value on every page.
fn hue(value: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h % 360
}

/// The answers each lane gave, each the first time it was given.
fn dots(model: &RunModel, lanes: &[Lane]) -> Vec<Dot> {
    let mut seen: BTreeSet<(u32, Option<String>, String)> = BTreeSet::new();
    let mut out = Vec::new();
    for event in &model.events {
        let EventKind::SdkRecord { index } = event.kind else { continue };
        let Some(Record::Event(name, details, _)) = model.records.get(index as usize) else {
            continue;
        };
        if name != bohr_sdk_proto::probe::PROBE_EVENT {
            continue;
        }
        let Some(round) = bohr_sdk_proto::probe::Round::from_details(details) else { continue };
        for (request, answer) in &round.0 {
            let Some(lane) = lanes.get(*request) else { continue };
            let bohr_sdk_proto::probe::Answer::Result(result) = answer else { continue };
            let Some(value) = result.get(&lane.value).map(text_of) else { continue };
            let key = lane.key.as_ref().and_then(|k| result.get(k)).map(text_of);
            if seen.insert((lane.node, key.clone(), value.clone())) {
                out.push(Dot { tick: event.tick.get(), node: lane.node, key, value });
            }
        }
    }
    out
}

/// The stretches each node was dead, as (node, from, to).
fn outages(model: &RunModel, end: u64) -> Vec<(u32, u64, u64)> {
    let mut open: Vec<(u32, u64)> = Vec::new();
    let mut out = Vec::new();
    for event in &model.events {
        let EventKind::NodeState { state } = event.kind else { continue };
        let node = event.node.get();
        match state {
            NodeState::Dead | NodeState::Frozen => {
                if !open.iter().any(|(n, _)| *n == node) {
                    open.push((node, event.tick.get()));
                }
            }
            NodeState::Restarting | NodeState::Running => {
                if let Some(at) = open.iter().position(|(n, _)| *n == node) {
                    let (_, from) = open.remove(at);
                    out.push((node, from, event.tick.get()));
                }
            }
        }
    }
    out.extend(open.into_iter().map(|(node, from)| (node, from, end)));
    out
}

/// Where the violation was seen: the first diagnostic of a broken probe property.
///
/// A notice that shares the codes' range, such as a full key table, is not a violation.
fn seen_at(events: &[bohr_trace::Event]) -> Option<u64> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::Diagnostic { code, .. } if bohr_trace::PROBE_CODES.contains(&code.as_str()) => {
            Some(e.tick.get())
        }
        _ => None,
    })
}

/// The timeline as an inline SVG, or nothing where no lane was answered.
#[must_use]
pub fn svg(model: &RunModel, lanes: &[Lane]) -> Option<String> {
    let dots = dots(model, lanes);
    if dots.is_empty() {
        return None;
    }
    let end = model.events.last().map_or(1, |e| e.tick.get()).max(1);
    let nodes: Vec<u32> =
        lanes.iter().map(|l| l.node).collect::<BTreeSet<_>>().into_iter().collect();
    let top_key = dots.iter().filter_map(|d| d.key.as_deref().and_then(height)).max().unwrap_or(0);
    let plot = WIDTH - LEFT - RIGHT;
    let x = |tick: u64| {
        LEFT + u64::try_from(u128::from(tick) * u128::from(plot) / u128::from(end)).unwrap_or(plot)
    };
    let lane_top = |node: u32| {
        let row = nodes.iter().position(|n| *n == node).unwrap_or(0) as u64;
        TOP + row * (LANE + GAP)
    };
    let height_px = TOP + nodes.len() as u64 * (LANE + GAP) + 24;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "<svg class=\"chain\" viewBox=\"0 0 {WIDTH} {height_px}\" width=\"100%\" role=\"img\" \
         aria-label=\"what each node answered the probe, over virtual time\">"
    );
    for node in &nodes {
        let top = lane_top(*node);
        let _ = writeln!(
            out,
            "<rect x=\"{LEFT}\" y=\"{top}\" width=\"{plot}\" height=\"{LANE}\" fill=\"#f6f6f4\"/>\
             <text x=\"{}\" y=\"{}\" font-size=\"12\" text-anchor=\"end\">node {node}</text>",
            LEFT - 8,
            top + LANE / 2 + 4
        );
    }
    for (node, from, to) in outages(model, end) {
        if !nodes.contains(&node) {
            continue;
        }
        let (a, b) = (x(from), x(to).max(x(from) + 1));
        let _ = writeln!(
            out,
            "<rect x=\"{a}\" y=\"{}\" width=\"{}\" height=\"{LANE}\" fill=\"#f3c7c7\">\
             <title>node {node} down from {} to {}</title></rect>",
            lane_top(node),
            b - a,
            crate::narrate::seconds(from),
            crate::narrate::seconds(to)
        );
    }
    for dot in &dots {
        let top = lane_top(dot.node);
        let y = match dot.key.as_deref().and_then(height) {
            Some(h) if top_key > 0 => top + LANE - 6 - h * (LANE - 12) / top_key,
            _ => top + LANE / 2,
        };
        let label = format!(
            "node {} · {}{} · {}",
            dot.node,
            dot.key.as_ref().map_or_else(String::new, |k| format!("{k} → ")),
            dot.value,
            crate::narrate::seconds(dot.tick)
        );
        let _ = writeln!(
            out,
            "<circle cx=\"{}\" cy=\"{y}\" r=\"3\" fill=\"hsl({}, 62%, 42%)\"><title>{}</title></circle>",
            x(dot.tick),
            hue(&dot.value),
            escape::text(&label)
        );
    }
    if let Some(at) = seen_at(&model.events) {
        let _ = writeln!(
            out,
            "<line x1=\"{0}\" x2=\"{0}\" y1=\"{1}\" y2=\"{2}\" stroke=\"#b00\" stroke-dasharray=\"4 3\"/>\
             <text x=\"{3}\" y=\"{4}\" font-size=\"12\" fill=\"#b00\">violation seen at {5}</text>",
            x(at),
            TOP - 6,
            height_px - 20,
            x(at) + 4,
            TOP - 10,
            crate::narrate::seconds(at)
        );
    }
    for i in 0..=5u64 {
        let tick = end * i / 5;
        let _ = writeln!(
            out,
            "<text x=\"{}\" y=\"{}\" font-size=\"11\" text-anchor=\"middle\">{}</text>",
            x(tick),
            height_px - 6,
            crate::narrate::seconds(tick)
        );
    }
    out.push_str("</svg>\n");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full key table, numbered next to the probe's properties, is not where a violation was
    /// seen.
    #[test]
    fn a_capacity_notice_is_not_a_violation_seen() {
        use bohr_trace::Event;
        use bohr_types::{EventId, NodeId, TaskId, Tick};
        let said = |tick: u64, code: &str| Event {
            id: EventId(tick),
            tick: Tick(tick),
            node: NodeId(0),
            task: TaskId(0),
            kind: EventKind::Diagnostic { code: code.to_owned(), message: String::new() },
        };
        let events = [said(5, "BOHR-2045"), said(9, bohr_trace::UNIQUE_CODE)];
        assert_eq!(seen_at(&events), Some(9));
    }

    #[test]
    fn a_key_reads_as_a_height_in_either_base() {
        assert_eq!(height("0x1f"), Some(31));
        assert_eq!(height("31"), Some(31));
        assert_eq!(height("latest"), None);
        assert_eq!(hue("0xab"), hue("0xab"), "one value, one colour");
    }
}
