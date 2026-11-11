//! Judging a single run from what it recorded, and a set of runs against each other.
//!
//! Beside the models, so an oracle and the diagnostics it reads change together.

use bohr_kernel::{Oracle, RunOutcome, Verdict};

/// Decides whether a set of runs, one tape driving several implementations of one specification,
/// counts as a finding.
///
/// A port of its own, with no default answer: an oracle either compares a group or is not one,
/// so nothing that composes oracles can stop comparing by leaving a method out.
pub trait GroupOracle {
    /// Stable identity, recorded in the bundle and checked before a replay.
    fn identity(&self) -> OracleId;

    /// Judges the runs against each other: `Agree`, a finding, or `Skipped` where nothing was
    /// compared. Implementations of one specification must agree; a disagreement is a bug in one.
    fn judge_group(&mut self, runs: &[RunOutcome]) -> Verdict;
}
use bohr_trace::Outcome;
use bohr_types::OracleId;

/// Reports a failed assertion, then a signal, then a non-zero exit status.
///
/// An assertion is the sharpest answer; a signal comes before a status, since a killed task has
/// none. Domain-free: every guest has diagnostics and a status.
#[derive(Clone, Copy, Debug, Default)]
pub struct PropertiesAndCrashOracle;

impl Oracle for PropertiesAndCrashOracle {
    fn identity(&self) -> OracleId {
        // Recorded in every bundle; changing it refuses all stored bundles.
        OracleId::new("properties+crash", 1)
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        let violation = run.diagnostics.iter().find(|d| d.code == crate::sdk_violation_code());
        if let Some(violation) = violation {
            // The model's words, naming the promise and its location.
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
/// Reads the fact the world recorded and decides it is a finding.
#[derive(Clone, Copy, Debug, Default)]
pub struct LivenessOracle;

impl Oracle for LivenessOracle {
    fn identity(&self) -> OracleId {
        OracleId::new("liveness", 1)
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        // Every silent node, since the set of nodes is the finding's identity.
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
/// Reads the fact the world recorded; "answered" is defined in `watch::Durability`.
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

/// Reports a law of durability or recovery a family's records broke (R1.1–R1.4).
///
/// Reads the codes the world's model watch raised; the sharpest law first, so a torn batch is told
/// as one and not as each of its keys lost.
#[derive(Clone, Copy, Debug, Default)]
pub struct LawOracle;

impl Oracle for LawOracle {
    fn identity(&self) -> OracleId {
        OracleId::new("laws", 4)
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        for code in bohr_trace::LAW_CODES {
            // A probe raises the same codes for its own properties; a law's message names its law.
            let broken: Vec<&str> = run
                .diagnostics
                .iter()
                .filter(|d| d.code == code && is_law(&d.message))
                .map(|d| d.message.as_str())
                .collect();
            if broken.is_empty() {
                continue;
            }
            let detail = format!("{code}: {}", broken.join("; "));
            // Durability, recovery, the ledger and resources, R1, R3.2 and R4 to R7, are a
            // durability outcome; agreement and moving again, R2 and R3.1, a property the replicas'
            // answers broke.
            let durability = [
                bohr_trace::ACKED_SURVIVES_CODE,
                bohr_trace::DELETED_STAYS_CODE,
                bohr_trace::BATCH_WHOLE_CODE,
                bohr_trace::RESTARTS_CODE,
                bohr_trace::ONCE_CODE,
                bohr_trace::CONSERVED_CODE,
                bohr_trace::INCLUDED_CODE,
                bohr_trace::FAILED_ACKED_CODE,
                bohr_trace::RESOURCE_BACK_CODE,
            ];
            return Verdict::Finding(if durability.contains(&code) {
                Outcome::DurabilityViolated(detail)
            } else {
                Outcome::ProbeViolated(detail)
            });
        }
        Verdict::Clean
    }
}

/// Whether a diagnostic was raised by a law of the family layer, which names its law first.
fn is_law(message: &str) -> bool {
    ["R1.", "R2.", "R3.", "R4.", "R5.", "R6.", "R7."].iter().any(|law| message.starts_with(law))
}

/// Reports a property a probe's answers broke.
///
/// One per property, reading the code the world raised.
#[derive(Clone, Copy, Debug)]
pub struct ProbeOracle(pub crate::Property);

impl Oracle for ProbeOracle {
    fn identity(&self) -> OracleId {
        OracleId::new(self.0.as_str(), 1)
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        let code = self.0.code();
        let broken: Vec<&str> =
            run.diagnostics.iter().filter(|d| d.code == code).map(|d| d.message.as_str()).collect();
        if broken.is_empty() {
            return Verdict::Clean;
        }
        Verdict::Finding(Outcome::ProbeViolated(format!("{code}: {}", broken.join("; "))))
    }
}

/// Reports implementations of one specification that did not produce the same thing.
///
/// Judges a group by what each execution projects.
///
/// Implementations share a tape but not conditions, so the projection must be a value the
/// specification fixes under any schedule. See `docs/ARCHITECTURE.md` §5.20.
#[derive(Clone, Copy, Debug, Default)]
pub struct AgreementOracle;

impl GroupOracle for AgreementOracle {
    fn identity(&self) -> OracleId {
        OracleId::new("agreement", 1)
    }

    fn judge_group(&mut self, runs: &[RunOutcome]) -> Verdict {
        // One execution is not a comparison.
        if runs.len() < 2 {
            return Verdict::Skipped;
        }
        // Every label any execution produced.
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
            // A label missing from some execution was not compared.
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
/// `properties+crash` is asked first, as the sharpest answer.
///
/// The identity is derived from the members; a panel of one has that member's identity.
#[allow(missing_debug_implementations)] // trait objects, none of which are Debug
pub struct Panel {
    members: Vec<Box<dyn Oracle>>,
}

impl Panel {
    /// A panel of the oracles a scenario asked for.
    ///
    /// The first member is separate, so a panel is never empty.
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
                // Clean only if every member says clean.
                Verdict::Clean | Verdict::Agree | Verdict::Skipped => {}
            }
        }
        Verdict::Clean
    }
}

/// What a scenario asks to be judged on, beyond the crash oracle every run gets.
///
/// A value, so the CLI and tests compose panels through one function.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Watching {
    /// Whether a healed cluster that commits nothing is a finding.
    pub liveness: bool,
    /// Whether an answer given before a sync and lost to a crash is a finding.
    pub durability: bool,
    /// Whether the laws of durability and recovery judge the scenario's `[model]`.
    pub laws: bool,
    /// Which properties the scenario's probes hold the answers to.
    pub probed: Probed,
}

/// The properties some probe of a scenario holds, each judged by its own oracle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // one flag per property, read independently
pub struct Probed {
    /// `never-reverts`, `BOHR-2040`.
    pub never_reverts: bool,
    /// `monotonic`, `BOHR-2044`.
    pub monotonic: bool,
    /// `agrees`, `BOHR-2041`.
    pub agrees: bool,
    /// `advances`, `BOHR-2042`.
    pub advances: bool,
    /// `unique`, `BOHR-2043`.
    pub unique: bool,
}

/// The oracles a scenario asks for, in the order they are asked.
///
/// From the configuration, never a flag, so a replay rebuilds the panel from the stored text.
#[must_use]
pub fn panel(watching: Watching) -> Panel {
    let mut rest: Vec<Box<dyn Oracle>> = Vec::new();
    // Durability before liveness, as the sharper answer.
    if watching.durability {
        rest.push(Box::new(DurabilityOracle));
    }
    // Sharpest first: double vote, revert, disagreement, regression, stall.
    let probed = watching.probed;
    for (on, property) in [
        (probed.unique, crate::Property::Unique),
        (probed.never_reverts, crate::Property::NeverReverts),
        (probed.agrees, crate::Property::Agrees),
        (probed.monotonic, crate::Property::Monotonic),
        (probed.advances, crate::Property::Advances),
    ] {
        if on {
            rest.push(Box::new(ProbeOracle(property)));
        }
    }
    if watching.liveness {
        rest.push(Box::new(LivenessOracle));
    }
    // A law is told before a crash: a node that refuses to come back exits non-zero, and the crash
    // would hide the law it broke.
    if watching.laws {
        rest.insert(0, Box::new(PropertiesAndCrashOracle));
        return Panel::of(Box::new(LawOracle), rest);
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

    /// A panel of one has its member's identity.
    #[test]
    fn a_panel_of_one_is_named_after_it() {
        let alone = Panel::of(Box::new(PropertiesAndCrashOracle), Vec::new());
        assert_eq!(alone.identity(), OracleId::new("properties+crash", 1));
    }

    /// A panel of several has a `panel(...)` identity no oracle can have.
    #[test]
    fn a_panel_earns_a_name_no_single_oracle_can_wear() {
        let panel =
            Panel::of(Box::new(PropertiesAndCrashOracle), vec![Box::new(AlwaysFinds("second"))]);
        assert_eq!(panel.identity().name, "panel(properties+crash@1,always-finds@1)");
        assert_ne!(panel.identity(), OracleId::new("properties+crash", 1));
    }

    /// The first finding wins.
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

    /// A group agrees when every execution projected the same, disagrees on any label that
    /// differed, and is skipped where nothing was compared.
    #[test]
    fn a_group_is_judged_by_what_its_executions_projected() {
        let run = |value: &[u8]| RunOutcome {
            digest: bohr_types::Digest::of(b"run"),
            events: 0,
            ticks: bohr_types::Tick(0),
            exit: Some(0),
            killed_by: None,
            diagnostics: Vec::new(),
            projections: vec![("state root".to_owned(), bohr_types::Digest::of(value))],
        };
        let mut oracle = AgreementOracle;
        assert_eq!(oracle.judge_group(&[run(b"a"), run(b"a")]), Verdict::Agree);
        assert!(matches!(
            oracle.judge_group(&[run(b"a"), run(b"b")]),
            Verdict::Finding(Outcome::ImplementationsDisagreed(label)) if label == "state root"
        ));
        assert_eq!(oracle.judge_group(&[run(b"a")]), Verdict::Skipped, "one is no comparison");
    }
}
