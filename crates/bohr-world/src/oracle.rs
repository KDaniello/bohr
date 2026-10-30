//! Judging a single run from what it recorded.
//!
//! Beside the models rather than in the tool that wires them: the diagnostic this reads is raised
//! a few modules away, and an oracle that lives in another crate from the vocabulary it reads is
//! one rename away from silently judging nothing.

use bohr_kernel::{Oracle, RunOutcome, Verdict};
use bohr_trace::Outcome;
use bohr_types::OracleId;

/// Reports a failed assertion, then a signal, then a non-zero exit status.
///
/// In that order deliberately. A guest that asserts something about itself has said what it means
/// by working, and that is a sharper answer than the exit status it happened to reach afterwards;
/// a signal comes next because a task killed by one never reached a status at all.
///
/// This oracle knows nothing about any domain: it reads the diagnostics the run recorded and the
/// status the guest ended with, both of which every guest has.
#[derive(Clone, Copy, Debug, Default)]
pub struct PropertiesAndCrashOracle;

impl Oracle for PropertiesAndCrashOracle {
    fn identity(&self) -> OracleId {
        // Recorded in every bundle and checked before a replay. Changing either the name or the
        // number refuses every bundle ever written.
        OracleId::new("properties+crash", 1)
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        let violation = run.diagnostics.iter().find(|d| d.code == crate::sdk_violation_code());
        if let Some(violation) = violation {
            // The model's own words, which name the promise and where it is written. A verdict
            // that said only "an assertion did not hold" would be the same sentence for every
            // assertion any guest will ever write, and a bundle attached to a bug report would
            // not say what the bug is.
            return Verdict::Finding(Outcome::PropertyViolated(violation.message.clone()));
        }
        if let Some(signal) = run.killed_by {
            return Verdict::Finding(Outcome::GuestCrashed(format!("killed by signal {signal}")));
        }
        match run.exit {
            Some(0) | None => Verdict::Clean,
            Some(code) => Verdict::Finding(Outcome::GuestCrashed(format!("exit status {code}"))),
        }
    }
}

/// Reports a cluster that could talk to itself again and went on committing nothing.
///
/// Reads a fact the world recorded while the run was happening, exactly as the crash oracle reads
/// the one an SDK assertion leaves. It does not decide what "committed" means and it does not
/// watch anything: the world states the fact and gives it a code, and this decides the fact is a
/// finding. Nothing here knows what a partition is.
#[derive(Clone, Copy, Debug, Default)]
pub struct LivenessOracle;

impl Oracle for LivenessOracle {
    fn identity(&self) -> OracleId {
        OracleId::new("liveness", 1)
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        // Every silent node, not the first: a cluster where one node went quiet and a cluster
        // where all five did are different findings, and the reduction's identity is which nodes.
        let silent: Vec<&str> = run
            .diagnostics
            .iter()
            .filter(|d| d.code == bohr_trace::LIVENESS_CODE)
            .map(|d| d.message.as_str())
            .collect();

        if silent.is_empty() {
            return Verdict::Clean;
        }
        Verdict::Finding(Outcome::LivenessViolated(silent.join("; ")))
    }
}

/// Reports a node that answered somebody before it synced and came back without the data.
///
/// Reads the fact the world recorded and decides it is a finding. What "answered" means is the
/// world's approximation and is documented where it is made rather than here — an oracle that
/// re-derived it would be a second definition, and the two would drift.
#[derive(Clone, Copy, Debug, Default)]
pub struct DurabilityOracle;

impl Oracle for DurabilityOracle {
    fn identity(&self) -> OracleId {
        OracleId::new("durability", 1)
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        let lost: Vec<&str> = run
            .diagnostics
            .iter()
            .filter(|d| d.code == bohr_trace::DURABILITY_CODE)
            .map(|d| d.message.as_str())
            .collect();

        if lost.is_empty() {
            return Verdict::Clean;
        }
        Verdict::Finding(Outcome::DurabilityViolated(lost.join("; ")))
    }
}

/// Reports implementations of one specification that did not produce the same thing.
///
/// Judges a *group*, which is the one oracle here that does: `judge` has no opinion about a single
/// run, because one implementation cannot disagree with anything.
///
/// **What makes this sound is the projection, not the tape.** A differential run does not give the
/// implementations equal conditions — they make different syscalls in a different order, so every
/// draw whose position depends on guest behaviour lands differently for each. What they share is
/// one *source of adversity*. So the value they are compared by must be one the specification
/// requires to be equal under any legal schedule; comparing anything else would report a
/// disagreement on nearly every seed. See `docs/ARCHITECTURE.md` §5.20.
#[derive(Clone, Copy, Debug, Default)]
pub struct AgreementOracle;

impl Oracle for AgreementOracle {
    fn identity(&self) -> OracleId {
        OracleId::new("agreement", 1)
    }

    fn judge(&mut self, _run: &RunOutcome) -> Verdict {
        Verdict::Skipped
    }

    fn judge_group(&mut self, runs: &[RunOutcome]) -> Verdict {
        // One execution has nothing to disagree with, and calling that agreement would be the
        // trap this oracle exists to avoid, one layer lower down: a comparison that did not
        // happen is not a comparison that passed.
        if runs.len() < 2 {
            return Verdict::Skipped;
        }
        // Every label any execution produced, so one that produced nothing under a label the
        // others did is a question left unanswered rather than an agreement.
        let mut labels: Vec<&str> =
            runs.iter().flat_map(|r| r.projections.iter().map(|(l, _)| l.as_str())).collect();
        labels.sort_unstable();
        labels.dedup();

        if labels.is_empty() {
            return Verdict::Skipped;
        }

        let mut differed = Vec::new();
        for label in labels {
            let mut seen: Vec<Option<&bohr_types::Digest>> = Vec::with_capacity(runs.len());
            for run in runs {
                seen.push(run.projections.iter().find(|(l, _)| l == label).map(|(_, d)| d));
            }
            // A label some execution did not produce is not a disagreement and is not agreement
            // either: it is a comparison that did not happen, and reporting it as either would be
            // a campaign quietly measuring nothing.
            if seen.iter().any(Option::is_none) {
                return Verdict::Skipped;
            }
            if seen.windows(2).any(|pair| pair[0] != pair[1]) {
                differed.push(label.to_owned());
            }
        }

        if differed.is_empty() {
            Verdict::Agree
        } else {
            Verdict::Finding(Outcome::ImplementationsDisagreed(differed.join(", ")))
        }
    }
}

/// Every oracle judging one run, asked in order, first finding wins.
///
/// Order is not arbitrary and is the panel's only rule: `properties+crash` is asked first, because
/// a guest that crashed or broke a promise it made about itself is a sharper answer than a window
/// that stayed quiet — the same argument the crash oracle already makes internally about
/// assertions before exit statuses.
///
/// The identity is derived from the members rather than declared, so a panel cannot claim a
/// judgement it did not make. A panel of one is named exactly like that one, which is what keeps
/// every bundle written before panels existed replaying unchanged.
#[allow(missing_debug_implementations)] // trait objects, none of which are Debug
pub struct Panel {
    members: Vec<Box<dyn Oracle>>,
}

impl Panel {
    /// A panel of the oracles a scenario asked for.
    ///
    /// The first member is taken separately so an empty panel cannot be written: one that judged
    /// nothing would call every run clean, and would say so in an identity nobody could tell from
    /// a real one.
    #[must_use]
    pub fn of(first: Box<dyn Oracle>, rest: Vec<Box<dyn Oracle>>) -> Self {
        let mut members = Vec::with_capacity(rest.len() + 1);
        members.push(first);
        members.extend(rest);
        Self { members }
    }
}

impl Oracle for Panel {
    fn identity(&self) -> OracleId {
        OracleId::composed(&self.members.iter().map(|m| m.identity()).collect::<Vec<_>>())
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        for member in &mut self.members {
            match member.judge(run) {
                Verdict::Finding(outcome) => return Verdict::Finding(outcome),
                // A member with no opinion is not a member that found nothing: the next one is
                // still asked either way, and only a run every member called clean is clean.
                Verdict::Clean | Verdict::Agree | Verdict::Skipped => {}
            }
        }
        Verdict::Clean
    }

    /// The same rule for a group, and it has to be stated rather than inherited.
    ///
    /// A composite that answered only half its interface would take the default — no opinion —
    /// however many members held one, so a panel containing a comparison would quietly stop
    /// comparing. `Skipped` is kept where every member skips, because a group nobody had an
    /// opinion about is not a group that agreed.
    fn judge_group(&mut self, runs: &[RunOutcome]) -> Verdict {
        let mut answered = false;
        for member in &mut self.members {
            match member.judge_group(runs) {
                Verdict::Finding(outcome) => return Verdict::Finding(outcome),
                Verdict::Clean | Verdict::Agree => answered = true,
                Verdict::Skipped => {}
            }
        }
        if answered {
            Verdict::Agree
        } else {
            Verdict::Skipped
        }
    }
}

/// What a scenario asks to be judged on, beyond the crash oracle every run gets.
///
/// A value rather than the whole configuration, so the one place that composes a panel can be
/// called by the tool that reads a scenario file and by a test that builds one in memory. Two
/// places composing panels would be two places that can disagree about what a bundle was judged
/// by, and the disagreement is only visible when a replay refuses a bundle that was fine.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Watching {
    /// Whether a healed cluster that commits nothing is a finding.
    pub liveness: bool,
    /// Whether an answer given before a sync and lost to a crash is a finding.
    pub durability: bool,
}

/// The oracles a scenario asks for, in the order they are asked.
///
/// Chosen by configuration and never by a flag on the command line. The conditions these oracles
/// judge are parameterised — how long a silence has to last, what counts as an answer — and a flag
/// would let a replay judge with a different parameter than the bundle recorded: a run that is
/// self-consistent and wrong, which is exactly what `check_provenance` exists to prevent. The
/// configuration text is stored in the bundle verbatim, so a replay reconstructs the world *and*
/// the panel from the same bytes.
#[must_use]
pub fn panel(watching: Watching) -> Panel {
    let mut rest: Vec<Box<dyn Oracle>> = Vec::new();
    // Durability before liveness: a node that lost an acknowledged write is a sharper answer than
    // a cluster that went quiet, and a cluster whose node lost data is usually both.
    if watching.durability {
        rest.push(Box::new(DurabilityOracle));
    }
    if watching.liveness {
        rest.push(Box::new(LivenessOracle));
    }
    Panel::of(Box::new(PropertiesAndCrashOracle), rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Judges every run a finding, so a panel's order is visible in what comes out.
    struct AlwaysFinds(&'static str);

    impl Oracle for AlwaysFinds {
        fn identity(&self) -> OracleId {
            OracleId::new("always-finds", 1)
        }

        fn judge(&mut self, _run: &RunOutcome) -> Verdict {
            Verdict::Finding(Outcome::InternalError(self.0.to_owned()))
        }
    }

    /// The property every stored bundle depends on: a panel of one is named exactly like that one,
    /// so a build that grew the concept of panels still replays what a build without it wrote.
    #[test]
    fn a_panel_of_one_is_named_after_it() {
        let alone = Panel::of(Box::new(PropertiesAndCrashOracle), Vec::new());
        assert_eq!(alone.identity(), OracleId::new("properties+crash", 1));
    }

    /// And a panel of several says so, so a bundle judged by more cannot be replayed by fewer.
    ///
    /// The name is a shape no oracle can be declared with, which matters here more than anywhere:
    /// this panel's first member is called `properties+crash`, so a name joined with `+` gave the
    /// panel a key that an oracle already in this file could wear.
    #[test]
    fn a_panel_earns_a_name_no_single_oracle_can_wear() {
        let panel =
            Panel::of(Box::new(PropertiesAndCrashOracle), vec![Box::new(AlwaysFinds("second"))]);
        assert_eq!(panel.identity().name, "panel(properties+crash@1,always-finds@1)");
        assert_ne!(panel.identity(), OracleId::new("properties+crash", 1));
    }

    /// First finding wins, and the order is the panel's only rule.
    #[test]
    fn the_first_member_to_find_something_decides() {
        let mut panel =
            Panel::of(Box::new(AlwaysFinds("first")), vec![Box::new(AlwaysFinds("second"))]);
        let run = RunOutcome {
            digest: bohr_types::Digest::of(b""),
            events: 0,
            ticks: bohr_types::Tick(0),
            exit: Some(0),
            killed_by: None,
            diagnostics: Vec::new(),
            projections: Vec::new(),
        };
        assert_eq!(panel.judge(&run), Verdict::Finding(Outcome::InternalError("first".to_owned())));
    }
}
