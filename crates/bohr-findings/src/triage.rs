//! What a finding is: a defect of the target, or bohr's own limit, the bench's failure, a mode
//! nobody runs in production, the model's artefact, or a refusal the target was right to make.
//!
//! The decision is by rule and in order, the first that applies (`docs/API.md` §7.7): a bench
//! failure under a model-only fault is a bench failure. Only a target defect counts; every other
//! verdict keeps the finding and lists it apart.

use bohr_trace::{Bundle, FaultGrade, FaultKind, Outcome};

use crate::ablation::Ablation;

/// What a finding is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The target broke under faults a stock host produces, or under none.
    TargetDefect,
    /// The target broke, under faults of this grade: plausible or adversarial.
    Conditional(FaultGrade),
    /// The failure needs a fault no known host produces.
    ModelArtefact,
    /// The target stopped on data a fault destroyed after it was synced.
    ExpectedRefusal,
    /// Every failing node is part of the bench: this node, with this role.
    BenchFailure {
        /// The node.
        node: u32,
        /// What it is to the run.
        role: NodeRole,
    },
    /// The run used a mode the target's pack declares non-production: this word.
    OutOfScope {
        /// The word.
        word: String,
    },
    /// bohr stopped the run for a reason of its own.
    BohrLimit(String),
    /// Not ablated, so the faults it needed and their grade are not known.
    Unjudged,
}

impl Verdict {
    /// The verdict in words.
    pub fn name(&self) -> String {
        match self {
            Self::TargetDefect => "target defect".to_owned(),
            Self::Conditional(grade) => format!("target defect, {}", grade.name()),
            Self::ModelArtefact => "model artefact".to_owned(),
            Self::ExpectedRefusal => "expected refusal".to_owned(),
            Self::BenchFailure { node, role } => format!("bench failure: node {node}, {role}"),
            Self::OutOfScope { word } => format!("out of scope: {word}"),
            Self::BohrLimit(why) => format!("bohr limit: {why}"),
            Self::Unjudged => "not ablated".to_owned(),
        }
    }

    /// Whether the finding counts as a defect of the target.
    pub const fn counts(&self) -> bool {
        matches!(self, Self::TargetDefect | Self::Conditional(_))
    }
}

/// The verdict for one bundle, with its ablation where one was run.
///
/// `events` are the bundle's [`telling`](crate::group::telling) events, which the caller has
/// already read to group it.
///
/// `nonproduction` are the words the pack of the bundle's programs declares non-production, which
/// the caller looks up; none where the programs belong to no pack.
pub fn triage(
    bundle: &Bundle,
    events: &[bohr_trace::Event],
    ablation: Option<&Ablation>,
    nonproduction: &[String],
) -> Verdict {
    let Ok(manifest) = bundle.manifest() else {
        return Verdict::BohrLimit("the manifest is unreadable".to_owned());
    };
    if let Some(why) = bohr_limit(&manifest.outcome) {
        return Verdict::BohrLimit(why);
    }
    let Ok(config_text) = bundle.config_text() else {
        return Verdict::BohrLimit("the configuration is unreadable".to_owned());
    };
    let Ok(loaded) = bohr_world::config::parse(config_text) else {
        return Verdict::BohrLimit("the configuration does not parse".to_owned());
    };
    let cluster = loaded.cluster.as_ref();

    let failing = bohr_report::narrate::failing_nodes(events);
    let roles = cluster.map(|c| c.roles.clone()).unwrap_or_default();
    let nodes = cluster.map(|c| c.nodes);
    if !failing.is_empty()
        && let Some(bench) = failing
            .iter()
            .map(|n| (*n, role_of(&roles, nodes, *n)))
            .find(|(_, r)| *r != NodeRole::Target)
        && failing.iter().all(|n| role_of(&roles, nodes, *n) != NodeRole::Target)
    {
        return Verdict::BenchFailure { node: bench.0, role: bench.1 };
    }

    if let Some(word) = nonproduction_word(bundle, cluster, nonproduction) {
        return Verdict::OutOfScope { word };
    }

    let Some(ablation) = ablation else {
        return Verdict::Unjudged;
    };
    let necessary = ablation.necessary();
    // A restore from an older snapshot loses synced data, and a node that refuses to start on
    // it is right to.
    let restored = necessary.contains(&FaultKind::NodeRestart)
        || ablation.scheduled.contains(&FaultKind::NodeRestart);
    if restored && !failing.is_empty() {
        return Verdict::ExpectedRefusal;
    }
    // Faults a scenario writes down are needed by its definition, so their grades count too.
    let grade =
        ablation.grade().into_iter().chain(ablation.scheduled.iter().map(|k| k.grade())).max();
    match grade {
        None | Some(FaultGrade::Real) => Verdict::TargetDefect,
        Some(FaultGrade::ModelOnly) => Verdict::ModelArtefact,
        Some(grade) => Verdict::Conditional(grade),
    }
}

/// Why bohr, and not the target, ended a run, where it did.
fn bohr_limit(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::UnsupportedSyscall(what) => Some(format!("an unmodelled call: {what}")),
        Outcome::InternalError(what) => Some(format!("a supervisor error: {what}")),
        Outcome::StoppedOnRequest(_) => Some("stopped on request".to_owned()),
        Outcome::GuestHung(said) if said.contains("budget") => Some(format!("the {said}")),
        _ => None,
    }
}

/// What a node is to the run: the system under test, the probe, or a part of the bench the
/// configuration names, such as `load`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NodeRole {
    /// The system under test.
    Target,
    /// The probe that asks the nodes.
    Probe,
    /// A part of the bench, by the name the configuration gives it.
    Bench(String),
}

impl std::fmt::Display for NodeRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Target => write!(f, "target"),
            Self::Probe => write!(f, "probe"),
            Self::Bench(name) => write!(f, "{name}"),
        }
    }
}

/// A node's role: past the cluster's nodes it is the probe, of the bench; a node the
/// configuration names no role for, or names `target`, is a target.
fn role_of(roles: &[String], nodes: Option<u32>, node: u32) -> NodeRole {
    if nodes.is_some_and(|nodes| node >= nodes) {
        return NodeRole::Probe;
    }
    match roles.get(node as usize).map(String::as_str) {
        None | Some("target") => NodeRole::Target,
        Some(name) => NodeRole::Bench(name.to_owned()),
    }
}

/// A word of a recorded command line that is one of `nonproduction`, or sets one.
fn nonproduction_word(
    bundle: &Bundle,
    cluster: Option<&bohr_world::Cluster>,
    nonproduction: &[String],
) -> Option<String> {
    // A section that does not read is judged without its words.
    let binaries = bundle.binaries().unwrap_or_default();
    let mut words: Vec<String> = binaries.iter().flat_map(|b| b.args.clone()).collect();
    if let Some(cluster) = cluster {
        words.extend(cluster.argv.iter().cloned());
        words.extend(cluster.programs.iter().flatten().cloned());
    }
    words.into_iter().find_map(|word| {
        nonproduction.iter().find(|np| word == **np || word.starts_with(&format!("{np}="))).cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_left_out_is_a_target() {
        let roles = vec!["target".to_owned(), "load".to_owned()];
        assert_eq!(role_of(&roles, Some(5), 1), NodeRole::Bench("load".to_owned()));
        assert_eq!(role_of(&roles, Some(5), 0), NodeRole::Target);
        assert_eq!(role_of(&roles, Some(5), 4), NodeRole::Target);
        assert_eq!(role_of(&[], None, 0), NodeRole::Target);
    }

    /// The node past the cluster's is the probe, so its failure alone is the bench's.
    #[test]
    fn the_node_past_the_cluster_is_the_probe() {
        assert_eq!(role_of(&[], Some(3), 3), NodeRole::Probe);
        assert_eq!(role_of(&[], Some(3), 2), NodeRole::Target);
    }

    #[test]
    fn only_a_target_defect_counts() {
        assert!(Verdict::TargetDefect.counts());
        assert!(Verdict::Conditional(FaultGrade::Plausible).counts());
        assert!(!Verdict::ModelArtefact.counts());
        assert!(!Verdict::OutOfScope { word: "--dev".to_owned() }.counts());
        assert!(!Verdict::Unjudged.counts());
    }

    #[test]
    fn bohr_stopping_a_run_is_its_own_limit() {
        assert!(bohr_limit(&Outcome::UnsupportedSyscall("statx".into())).is_some());
        assert!(bohr_limit(&Outcome::GuestHung("event budget reached at 1000".into())).is_some());
        assert!(bohr_limit(&Outcome::GuestCrashed("exit status 1".into())).is_none());
    }
}
