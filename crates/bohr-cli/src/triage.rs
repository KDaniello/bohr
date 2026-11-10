//! What a finding is: a defect of the target, or bohr's own limit, the bench's failure, a mode
//! nobody runs in production, the model's artefact, or a refusal the target was right to make.
//!
//! The decision is by rule and in order, the first that applies (`docs/API.md` §7.7): a bench
//! failure under a model-only fault is a bench failure. Only a target defect counts; every other
//! verdict keeps the finding and lists it apart.

use bohr_trace::{Bundle, FaultGrade, FaultKind, Outcome, SectionId};

use crate::ablate::Ablation;

/// What a finding is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The target broke under faults a stock host produces, or under none.
    TargetDefect,
    /// The target broke, under faults of this grade: plausible or adversarial.
    Conditional(FaultGrade),
    /// The failure needs a fault no known host produces.
    ModelArtefact,
    /// The target stopped on data a fault destroyed after it was synced.
    ExpectedRefusal,
    /// Every failing node is part of the bench: this node, with this role.
    BenchFailure { node: u32, role: String },
    /// The run used a mode the target's pack declares non-production: this word.
    OutOfScope { word: String },
    /// bohr stopped the run for a reason of its own.
    BohrLimit(String),
    /// Not ablated, so the faults it needed and their grade are not known.
    Unjudged,
}

impl Verdict {
    /// The verdict in words.
    pub(crate) fn name(&self) -> String {
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
    pub(crate) const fn counts(&self) -> bool {
        matches!(self, Self::TargetDefect | Self::Conditional(_))
    }
}

/// The verdict for one bundle, with its ablation where one was run.
pub(crate) fn triage(bundle: &Bundle, ablation: Option<&Ablation>) -> Verdict {
    let Ok(manifest) = bundle.manifest() else {
        return Verdict::BohrLimit("the manifest is unreadable".to_owned());
    };
    if let Some(why) = bohr_limit(&manifest.outcome) {
        return Verdict::BohrLimit(why);
    }
    let events: Vec<bohr_trace::Event> = bundle
        .section(SectionId::Events)
        .map(|raw| bohr_trace::read_events_where(raw, crate::findings::telling).0)
        .unwrap_or_default();
    let config: toml::Table =
        String::from_utf8_lossy(bundle.section(SectionId::Config).unwrap_or_default())
            .parse()
            .unwrap_or_default();

    let failing = bohr_report::narrate::failing_nodes(&events);
    let roles = roles_of(&config);
    if !failing.is_empty()
        && let Some(bench) =
            failing.iter().map(|n| (*n, role_of(&roles, *n))).find(|(_, r)| r != "target")
        && failing.iter().all(|n| role_of(&roles, *n) != "target")
    {
        return Verdict::BenchFailure { node: bench.0, role: bench.1 };
    }

    if let Some(word) = nonproduction_word(bundle, &config) {
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

/// Each node's role as the recorded configuration names it.
fn roles_of(config: &toml::Table) -> Vec<String> {
    config
        .get("cluster")
        .and_then(|c| c.get("roles"))
        .and_then(toml::Value::as_array)
        .map(|roles| roles.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// A node's role: a probe past the named ones is the bench, an unnamed one a target.
fn role_of(roles: &[String], node: u32) -> String {
    roles.get(node as usize).cloned().unwrap_or_else(|| "target".to_owned())
}

/// A word of a recorded command line that the target's pack declares non-production.
fn nonproduction_word(bundle: &Bundle, config: &toml::Table) -> Option<String> {
    let binaries: Vec<bohr_trace::BinaryRecord> = bundle
        .section(SectionId::Binaries)
        .and_then(|raw| serde_json::from_slice(raw).ok())
        .unwrap_or_default();
    let digests: Vec<&str> = binaries.iter().map(|b| b.blake3.as_str()).collect();
    let pack = crate::targets::pack_of(&digests)?;
    let mut words: Vec<String> = binaries.iter().flat_map(|b| b.args.clone()).collect();
    let cluster = config.get("cluster");
    for key in ["argv", "programs"] {
        let mut stack: Vec<&toml::Value> = cluster.and_then(|c| c.get(key)).into_iter().collect();
        while let Some(value) = stack.pop() {
            match value {
                toml::Value::String(word) => words.push(word.clone()),
                toml::Value::Array(items) => stack.extend(items),
                _ => {}
            }
        }
    }
    words.into_iter().find_map(|word| {
        pack.nonproduction
            .iter()
            .find(|np| word == **np || word.starts_with(&format!("{np}=")))
            .cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_left_out_is_a_target() {
        let roles = vec!["target".to_owned(), "load".to_owned()];
        assert_eq!(role_of(&roles, 1), "load");
        assert_eq!(role_of(&roles, 4), "target");
        assert_eq!(role_of(&[], 0), "target");
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
