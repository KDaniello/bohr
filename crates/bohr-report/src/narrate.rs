//! What a run found, said in three levels: a verdict, the evidence before it, the commands next.
//!
//! **Readability here is not simplification, but order.** A reader wants first what broke and
//! where, then what led there, then the raw log; the log alone gives the third and leaves the
//! first two to be dug out of it. Every level keeps the codes and numbers of the one below — a
//! verdict without its code cannot be searched for, and a code without its verdict cannot be read.
//!
//! The evidence is gathered by rule, not by knowing the program: the faults, the node changes, the
//! failed tasks and the diagnostics recorded before the violation, nearest last.
//!
//! What this does not do: say why. Evidence is what came before the violation and touched the run;
//! that it caused it is not shown, and the story is titled "how it got there" for that reason.

use std::fmt::Write as _;

use bohr_trace::{Event, EventKind, ExitStatus, NodeState, Outcome};

use crate::{RunModel, describe};

/// How many steps a story shows at most: enough for a fault, a restart and what followed.
const MAX_STEPS: usize = 12;

/// What a run found, in one sentence and the code that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Whether anything was found.
    pub found: bool,
    /// The sentence: what broke, and where.
    pub headline: String,
    /// The diagnostic code or the outcome's kind, for a search.
    pub code: String,
}

/// One step of how a run got to what it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The event, as `bohr trace` numbers it.
    pub event: u64,
    /// Virtual time, in nanoseconds.
    pub tick: u64,
    /// The node it happened on.
    pub node: u32,
    /// What happened, in words.
    pub text: String,
    /// Whether this is the event the outcome points at.
    pub violation: bool,
}

/// The verdict, the steps before it, and what to type next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Story {
    pub verdict: Verdict,
    /// What the run was: seed, nodes, virtual time, faults.
    pub run: String,
    pub steps: Vec<Step>,
    /// Steps that were found and not shown, because only the nearest are.
    pub elided: usize,
    /// Commands that show more, each with what it shows.
    pub next: Vec<(String, &'static str)>,
}

/// A signal's name, where it is one a crash is commonly reported with.
fn signal_name(signal: u32) -> Option<&'static str> {
    Some(match signal {
        4 => "SIGILL",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// The `BOHR-NNNN` an outcome's words start with, where they do.
fn leading_code(detail: &str) -> Option<&str> {
    let code = detail.get(..9)?;
    (code.starts_with("BOHR-") && code[5..].chars().all(|c| c.is_ascii_digit())).then_some(code)
}

/// A probe's own words, `probe "name": rest`, as the name and the rest.
fn probe_words(message: &str) -> Option<(&str, &str)> {
    let rest = message.strip_prefix("probe \"")?;
    let (name, rest) = rest.split_once("\": ")?;
    Some((name, rest))
}

/// The verdict an outcome comes to, from the outcome alone.
///
/// One place for every kind, so a summary, a story and a page never word one finding two ways.
#[must_use]
pub fn verdict(outcome: &Outcome) -> Verdict {
    let kind = format!("{outcome:?}");
    let kind = kind.split('(').next().unwrap_or("").to_owned();
    let detail = describe::outcome_detail(outcome).unwrap_or("");
    let (headline, code) = match outcome {
        Outcome::Clean => ("nothing was found".to_owned(), kind),
        Outcome::GuestCrashed(detail) => {
            let said = match detail.strip_prefix("killed by signal ").and_then(|s| s.parse().ok()) {
                Some(signal) => match signal_name(signal) {
                    Some(name) => format!("a guest was killed by signal {signal} ({name})"),
                    None => format!("a guest was killed by signal {signal}"),
                },
                None => format!("a guest failed: {detail}"),
            };
            (said, kind)
        }
        Outcome::GuestHung(detail) => (format!("the run stopped making progress: {detail}"), kind),
        Outcome::PropertyViolated(detail) => (format!("a promise did not hold: {detail}"), kind),
        Outcome::LivenessViolated(detail) => {
            (format!("the cluster stopped committing: {detail}"), kind)
        }
        Outcome::DurabilityViolated(detail) => {
            (format!("an acknowledged write was lost: {detail}"), kind)
        }
        Outcome::ProbeViolated(detail) => {
            let code = leading_code(detail).map_or_else(|| kind.clone(), str::to_owned);
            let first = detail.get(code.len()..).unwrap_or("").trim_start_matches(": ");
            let first = first.split("; ").next().unwrap_or(first);
            let said = match probe_words(first) {
                Some((name, rest)) => format!("{name} broken: {rest}"),
                None => format!("a node's answers broke a property: {first}"),
            };
            (said, code)
        }
        Outcome::UnsupportedSyscall(detail) => {
            (format!("the guest asked for a call this build does not model: {detail}"), kind)
        }
        Outcome::ImplementationsDisagreed(detail) => {
            (format!("implementations disagree: {detail}"), kind)
        }
        Outcome::InternalError(detail) => (format!("bohr failed, not the guest: {detail}"), kind),
        Outcome::StoppedOnRequest(detail) => (format!("stopped where asked: {detail}"), kind),
    };
    let _ = detail;
    Verdict { found: !matches!(outcome, Outcome::Clean), headline, code }
}

/// Virtual time as a reader reads it: seconds, to the millisecond.
#[must_use]
pub fn seconds(tick: u64) -> String {
    format!("{}.{:03} s", tick / 1_000_000_000, tick / 1_000_000 % 1000)
}

/// What one event says as a step of a story, where it is one.
fn step_text(event: &Event) -> Option<String> {
    Some(match &event.kind {
        EventKind::FaultFired { fault, target } => {
            format!("{} ({})", describe::fault_action(*fault), describe::fault_target(target))
        }
        EventKind::NodeState { state } => match state {
            NodeState::Dead => format!("node {} is dead", event.node.get()),
            NodeState::Restarting => format!("node {} restarting", event.node.get()),
            NodeState::Frozen => format!("node {} frozen", event.node.get()),
            NodeState::Running => format!("node {} running again", event.node.get()),
        },
        EventKind::TaskExited { status: ExitStatus::Exited(code) } if *code != 0 => {
            format!("task {} exited with status {code}", event.task.get())
        }
        EventKind::TaskExited { status: ExitStatus::Signalled(signal) } => {
            match signal_name(*signal) {
                Some(name) => {
                    format!("task {} killed by signal {signal} ({name})", event.task.get())
                }
                None => format!("task {} killed by signal {signal}", event.task.get()),
            }
        }
        EventKind::Assertion { name, condition: false, kind, .. } => {
            format!("assertion \"{name}\" did not hold: it {}", describe::assertion_promise(*kind))
        }
        // Diagnostics a reader acts on, not the supervisor's notes about the machine it presented.
        EventKind::Diagnostic { code, message } if code != "BOHR-2007" => {
            format!("{code}: {message}")
        }
        _ => return None,
    })
}

/// The story of a stored run: its verdict, the steps before the violation, what to type next.
///
/// `bundle` is the path the commands in `next` name.
#[must_use]
pub fn story(model: &RunModel, bundle: &str) -> Story {
    let manifest = &model.manifest;
    let verdict = verdict(&manifest.outcome);
    // Where the violation was seen, not where it was judged: an oracle that reads a code judges at
    // the end of the run, and the first diagnostic carrying that code is the moment it happened.
    let seen = verdict.code.starts_with("BOHR-").then(|| {
        model.events.iter().find_map(|e| match &e.kind {
            EventKind::Diagnostic { code, .. } if *code == verdict.code => Some(e.id.get()),
            _ => None,
        })
    });
    let at = seen
        .flatten()
        .or(manifest.outcome_event)
        .or_else(|| model.violation.as_ref().map(|v| v.event.get()))
        .or_else(|| model.events.last().map(|e| e.id.get()));

    let mut steps: Vec<Step> = model
        .events
        .iter()
        .filter(|e| at.is_none_or(|at| e.id.get() <= at))
        .filter_map(|e| {
            step_text(e).map(|text| Step {
                event: e.id.get(),
                tick: e.tick.get(),
                node: e.node.get(),
                text,
                violation: Some(e.id.get()) == at,
            })
        })
        .collect();
    // The violation itself is a step even where nothing above names it: an exit the oracle judged
    // is the end of the story whether or not it left a diagnostic.
    if verdict.found
        && let Some(at) = at
        && !steps.iter().any(|s| s.violation)
        && let Some(event) = model.events.iter().find(|e| e.id.get() == at)
    {
        steps.push(Step {
            event: at,
            tick: event.tick.get(),
            node: event.node.get(),
            text: verdict.headline.clone(),
            violation: true,
        });
    }
    let elided = steps.len().saturating_sub(MAX_STEPS);
    steps.drain(..elided);

    let faults =
        model.events.iter().filter(|e| matches!(e.kind, EventKind::FaultFired { .. })).count();
    let last = model.events.last().map_or(0, |e| e.tick.get());
    let run = format!(
        "seed {} · {} node{} · {} virtual · {} event{} · {} fault{} landed",
        manifest.seed,
        model.nodes().len(),
        if model.nodes().len() == 1 { "" } else { "s" },
        seconds(last),
        model.events.len(),
        if model.events.len() == 1 { "" } else { "s" },
        faults,
        if faults == 1 { "" } else { "s" },
    );

    let mut next = Vec::new();
    if let (Some(first), Some(last)) = (steps.first(), steps.last()) {
        next.push((
            format!(
                "bohr trace {bundle} --from {} --to {}",
                seconds(first.tick).replace(' ', ""),
                seconds(last.tick + 1_000_000).replace(' ', "")
            ),
            "every event of that stretch",
        ));
    }
    if verdict.found {
        next.push((format!("bohr report {bundle}"), "one page, with the timeline"));
        next.push((
            format!("bohr reduce {bundle} -o smaller.bohr"),
            "the smallest run that still does this",
        ));
    }
    next.push((format!("bohr replay {bundle}"), "the same run again, checked against its digest"));
    Story { verdict, run, steps, elided, next }
}

/// A story as text, for `bohr explain`.
#[must_use]
pub fn write_story(story: &Story) -> String {
    let mut out = String::new();
    let v = &story.verdict;
    let _ = writeln!(out, "What was found\n  {} ({})\n", v.headline, v.code);
    let _ = writeln!(out, "What the run was\n  {}\n", story.run);
    if !story.steps.is_empty() {
        let _ = writeln!(out, "How it got there{:>56}", "event       time");
        if story.elided > 0 {
            let _ = writeln!(out, "  … {} earlier steps not shown", story.elided);
        }
        for (i, step) in story.steps.iter().enumerate() {
            let mark = if step.violation { "  ← found here" } else { "" };
            let text = format!("n{}  {}", step.node, step.text);
            let _ = writeln!(
                out,
                "  {:>2}  {:<58} {:>7}  {:>9}{mark}",
                i + 1,
                clip(&text, 58),
                step.event,
                seconds(step.tick)
            );
        }
        out.push('\n');
    }
    out.push_str("What to look at next\n");
    for (command, why) in &story.next {
        let _ = writeln!(out, "  {command}\n      {why}");
    }
    out.push_str(
        "\nThe steps are what came before the violation and touched the run, gathered by rule;\n\
         that they caused it is not shown, only that they came first.\n",
    );
    out
}

/// A story as one JSON object, with the same fields the text is written from.
#[must_use]
pub fn story_json(story: &Story) -> serde_json::Value {
    serde_json::json!({
        "found": story.verdict.found,
        "headline": story.verdict.headline,
        "code": story.verdict.code,
        "run": story.run,
        "elided": story.elided,
        "steps": story.steps.iter().map(|s| serde_json::json!({
            "event": s.event, "tick": s.tick, "node": s.node, "text": s.text,
            "violation": s.violation,
        })).collect::<Vec<_>>(),
        "next": story.next.iter().map(|(c, w)| serde_json::json!({ "command": c, "shows": w }))
            .collect::<Vec<_>>(),
    })
}

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind of outcome has a verdict of its own, and none of them reads like debug output.
    #[test]
    fn every_outcome_has_a_verdict_in_words() {
        let all = [
            Outcome::Clean,
            Outcome::PropertyViolated("x".into()),
            Outcome::GuestCrashed("exit status 1".into()),
            Outcome::GuestHung("no progress".into()),
            Outcome::LivenessViolated("x".into()),
            Outcome::DurabilityViolated("x".into()),
            Outcome::ProbeViolated(
                "BOHR-2040: probe \"finality\": node 2 answered a at 0x1, and later b".into(),
            ),
            Outcome::UnsupportedSyscall("x".into()),
            Outcome::ImplementationsDisagreed("x".into()),
            Outcome::InternalError("x".into()),
            Outcome::StoppedOnRequest("x".into()),
        ];
        for outcome in &all {
            let v = verdict(outcome);
            assert!(!v.headline.contains("(\"") && !v.headline.contains("\\\""), "{v:?}");
            assert!(!v.code.is_empty(), "{v:?}");
            assert_eq!(v.found, !matches!(outcome, Outcome::Clean));
        }
    }

    #[test]
    fn a_probe_verdict_names_the_probe_and_keeps_the_code() {
        let v = verdict(&Outcome::ProbeViolated(
            "BOHR-2040: probe \"finality\": node 2 answered 0xe4 at 0x1, and later 0xb4; probe \"x\": y".into(),
        ));
        assert_eq!(v.code, "BOHR-2040");
        assert_eq!(v.headline, "finality broken: node 2 answered 0xe4 at 0x1, and later 0xb4");
    }

    #[test]
    fn a_crash_by_signal_is_named() {
        let v = verdict(&Outcome::GuestCrashed("killed by signal 11".into()));
        assert_eq!(v.headline, "a guest was killed by signal 11 (SIGSEGV)");
        assert_eq!(v.code, "GuestCrashed");
        assert_eq!(seconds(1_331_000_000), "1.331 s");
    }
}
