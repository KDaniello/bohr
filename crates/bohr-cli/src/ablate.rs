//! `bohr ablate`: which faults a finding needed, and how real they are.
//!
//! The finding's run is repeated once per kind of fault that landed, with that kind's odds at
//! nought (`faults.ablate`). Every draw still happens, so the tape and every other fault stay where
//! they were. A kind whose removal makes the failure go, or turn into another, is necessary; the
//! finding's grade is the weakest grade among its necessary kinds.

use bohr_tape::{TAPE_ABI, Tape};
use bohr_trace::{Bundle, EventKind, FaultGrade, FaultKind, PacketFate, SectionId};

use crate::{CliError, execute, replay};

/// What removing one kind of fault did to the finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Without {
    /// The same failure: the kind was not needed.
    Same,
    /// No failure at all: the kind was needed.
    Clean,
    /// Another failure: the kind was needed for this one.
    Other(String),
}

/// One kind of fault, and what the run did without it.
#[derive(Clone, Debug)]
pub(crate) struct Trial {
    pub(crate) kind: FaultKind,
    pub(crate) without: Without,
}

/// What an ablation found.
#[derive(Clone, Debug)]
pub(crate) struct Ablation {
    /// The finding's signature, which every trial is compared against.
    pub(crate) signature: String,
    pub(crate) trials: Vec<Trial>,
    /// Where no single kind was needed and removing every drawn kind at once makes the failure go:
    /// the kinds of which any one suffices.
    pub(crate) alternatives: Vec<FaultKind>,
    /// Kinds that landed and that a scenario wrote down, which an ablation does not remove.
    pub(crate) scheduled: Vec<FaultKind>,
}

impl Ablation {
    /// The kinds the finding needed, every one of them.
    pub(crate) fn necessary(&self) -> Vec<FaultKind> {
        self.trials.iter().filter(|t| t.without != Without::Same).map(|t| t.kind).collect()
    }

    /// The finding's grade among its drawn faults: the weakest of the kinds it needed all of, or,
    /// where any one of several sufficed, the most real of those.
    pub(crate) fn grade(&self) -> Option<FaultGrade> {
        let needed = self.necessary();
        if needed.is_empty() {
            self.alternatives.iter().map(|k| k.grade()).min()
        } else {
            needed.into_iter().map(FaultKind::grade).max()
        }
    }
}

/// The kinds of fault that landed in a run, in the order they first landed.
///
/// A thaw is the end of a freeze and no fault of its own. A datagram dropped, duplicated,
/// reordered or corrupted by a draw lands as the packet's fate rather than as a fired fault, and
/// counts as its kind.
fn landed(events: &[bohr_trace::Event]) -> Vec<FaultKind> {
    let mut kinds = Vec::new();
    for event in events {
        let kind = match event.kind {
            EventKind::FaultFired { fault, .. } if fault != FaultKind::NodeThaw => Some(fault),
            EventKind::PacketFate { fate, .. } => match fate {
                PacketFate::Dropped => Some(FaultKind::NetDrop),
                PacketFate::Duplicated => Some(FaultKind::NetDuplicate),
                PacketFate::Reordered => Some(FaultKind::NetReorder),
                PacketFate::Corrupted => Some(FaultKind::NetCorrupt),
                PacketFate::Delivered => None,
            },
            _ => None,
        };
        if let Some(kind) = kind
            && !kinds.contains(&kind)
        {
            kinds.push(kind);
        }
    }
    kinds
}

/// The kinds a scenario writes down, which land whatever the draws say.
fn scheduled(config: &toml::Table) -> Vec<FaultKind> {
    let mut kinds = Vec::new();
    if config.get("partition").and_then(toml::Value::as_array).is_some_and(|p| !p.is_empty()) {
        kinds.push(FaultKind::NetPartition);
    }
    for fault in config.get("fault").and_then(toml::Value::as_array).into_iter().flatten() {
        let name = fault.get("kind").and_then(toml::Value::as_str).unwrap_or_default();
        let kind = match name {
            "node_kill" => Some(FaultKind::NodeKill),
            "node_freeze" | "node_thaw" => Some(FaultKind::NodeFreeze),
            // A plain restart is a recovery, not a fault; only a restore from a snapshot fires one.
            "node_restart_from_snapshot" => Some(FaultKind::NodeRestart),
            "clock_step" => Some(FaultKind::ClockStep),
            "clock_drift" => Some(FaultKind::ClockDrift),
            "chain_reorg" => Some(FaultKind::ChainReorg),
            _ => None,
        };
        if let Some(kind) = kind
            && !kinds.contains(&kind)
        {
            kinds.push(kind);
        }
    }
    kinds
}

/// The configuration with `kinds` added to `faults.ablate`.
fn without(config: &toml::Table, kinds: &[FaultKind]) -> Result<String, CliError> {
    let mut config = config.clone();
    let faults = config
        .entry("faults")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| CliError::Usage("BOHR-4002: the recorded faults are not a table".into()))?;
    let ablate = faults
        .entry("ablate")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| CliError::Usage("BOHR-4002: faults.ablate is not a list".into()))?;
    ablate.extend(kinds.iter().map(|kind| toml::Value::String(kind.name().to_owned())));
    toml::to_string(&config).map_err(|e| CliError::Internal(e.to_string()))
}

/// The bundle's run again with `kinds` at nought odds, and what it came to against `signature`.
fn trial(
    bundle: &Bundle,
    manifest: &bohr_trace::Manifest,
    tape: &Tape,
    config: &toml::Table,
    kinds: &[FaultKind],
    binaries: Option<&str>,
    signature: &str,
) -> Result<Without, CliError> {
    let text = without(config, kinds)?;
    let loaded = bohr_world::config::load(&text)?;
    let target = replay::target_from(bundle, manifest, Some(&loaded), binaries)?;
    let mut run_config = crate::run_config_for(Some(&loaded), None);
    run_config.max_events = run_config.max_events.max(manifest.counters.events + 1);
    let completed = execute::run_with(execute::Execution {
        target: &target,
        tape,
        config: run_config,
        output: execute::Output::Discard,
        policy: &manifest.policy.scheduler,
        // Spooled and read for the few events a signature needs, so a trial of a large run holds
        // none of its log.
        record: execute::Record::Spool,
        scenario: Some(&loaded),
        config_toml: &text,
    })?;
    if matches!(completed.report.outcome, bohr_trace::Outcome::Clean) {
        return Ok(Without::Clean);
    }
    let events = completed
        .spooled
        .as_ref()
        .and_then(|spooled| spooled.events_where(crate::findings::telling).ok())
        .map(|(events, _)| events)
        .unwrap_or_default();
    let found = crate::findings::signature_of(&completed.report.outcome, &events);
    Ok(if found == signature { Without::Same } else { Without::Other(found) })
}

/// Whether an event is one ablation reads: those a signature is read from, and every packet a
/// draw dropped, duplicated, reordered or corrupted.
fn ablation_reads(event: &bohr_trace::Event) -> bool {
    crate::findings::telling(event)
        || matches!(event.kind, EventKind::PacketFate { fate, .. } if fate != PacketFate::Delivered)
}

/// Ablates the finding in `bytes`, looking for moved binaries under `binaries`.
pub(crate) fn ablation(bytes: &[u8], binaries: Option<&str>) -> Result<Ablation, CliError> {
    let bundle = Bundle::parse(bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    ablation_of(&bundle, binaries)
}

/// Ablates a parsed finding, looking for moved binaries under `binaries`.
pub(crate) fn ablation_of(bundle: &Bundle, binaries: Option<&str>) -> Result<Ablation, CliError> {
    let manifest = bundle.manifest().map_err(|e| CliError::Usage(e.to_string()))?;
    manifest.check_tape_abi(TAPE_ABI).map_err(|e| CliError::Usage(e.to_string()))?;
    manifest.require_single_execution().map_err(|e| CliError::Usage(e.to_string()))?;
    replay::verify_binaries(bundle, binaries)?;

    let tape_bytes = bundle
        .section(SectionId::Tape)
        .ok_or_else(|| CliError::Usage("BOHR-4002: bundle is missing required section 2".into()))?;
    let tape = Tape::from_container(tape_bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    let events: Vec<bohr_trace::Event> = bundle
        .section(SectionId::Events)
        .map(|raw| bohr_trace::read_events_where(raw, ablation_reads).0)
        .unwrap_or_default();
    let signature = crate::findings::signature_of(&manifest.outcome, &events);

    let config_text =
        String::from_utf8_lossy(bundle.section(SectionId::Config).unwrap_or_default());
    let config: toml::Table = config_text
        .parse()
        .map_err(|e| CliError::Usage(format!("BOHR-4002: the recorded configuration: {e}")))?;
    let scheduled = scheduled(&config);

    let drawn: Vec<FaultKind> =
        landed(&events).into_iter().filter(|k| !scheduled.contains(k)).collect();
    let mut trials = Vec::new();
    for kind in &drawn {
        let without = trial(bundle, &manifest, &tape, &config, &[*kind], binaries, &signature)?;
        trials.push(Trial { kind: *kind, without });
    }
    // No single kind needed: any one of several may suffice, which removing them all tells.
    let mut alternatives = Vec::new();
    if drawn.len() > 1 && trials.iter().all(|t| t.without == Without::Same) {
        let all = trial(bundle, &manifest, &tape, &config, &drawn, binaries, &signature)?;
        if all != Without::Same {
            alternatives = drawn;
        }
    }
    Ok(Ablation { signature, trials, alternatives, scheduled })
}

/// `bohr ablate <bundle>`: prints which faults the finding needed and its grade.
pub(crate) fn ablate(path: &str, binaries: Option<&str>) -> Result<u8, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let ablation = ablation(&bytes, binaries)?;
    let necessary = ablation.necessary();
    if crate::json::on() {
        crate::json::emit(
            "ablate",
            serde_json::json!({
                "bundle": path,
                "signature": ablation.signature,
                "trials": ablation.trials.iter().map(|t| serde_json::json!({
                    "kind": t.kind.name(),
                    "grade": t.kind.grade().name(),
                    "without": match &t.without {
                        Without::Same => "same".to_owned(),
                        Without::Clean => "clean".to_owned(),
                        Without::Other(other) => format!("other: {other}"),
                    },
                })).collect::<Vec<_>>(),
                "necessary": necessary.iter().map(|k| k.name()).collect::<Vec<_>>(),
                "grade": ablation.grade().map(FaultGrade::name),
                "alternatives": ablation.alternatives.iter().map(|k| k.name()).collect::<Vec<_>>(),
                "scheduled": ablation.scheduled.iter().map(|k| k.name()).collect::<Vec<_>>(),
            }),
        );
        return Ok(crate::exit::CLEAN);
    }
    println!("{path}\n  {}", ablation.signature);
    if ablation.trials.is_empty() {
        println!("  no drawn fault landed; the finding needs none of them");
    }
    for trial in &ablation.trials {
        let what = match &trial.without {
            Without::Same => "still fails the same way: not needed".to_owned(),
            Without::Clean => "runs clean: needed".to_owned(),
            Without::Other(other) => format!("fails otherwise: needed for this one ({other})"),
        };
        println!("  without {:<18} {:<12} {what}", trial.kind.name(), trial.kind.grade().name());
    }
    for kind in &ablation.scheduled {
        println!("  {} is written in the scenario and is not ablated", kind.name());
    }
    let names =
        |kinds: &[FaultKind], by: &str| kinds.iter().map(|k| k.name()).collect::<Vec<_>>().join(by);
    match ablation.grade() {
        Some(grade) if !necessary.is_empty() => {
            println!("  needs {}: {}", names(&necessary, " + "), grade.name());
        }
        Some(grade) => {
            println!(
                "  needs any one of {}: {}",
                names(&ablation.alternatives, ", "),
                grade.name()
            );
        }
        None if !ablation.trials.is_empty() => {
            println!("  fails without every drawn fault: it needs none of them");
        }
        None => {}
    }
    Ok(crate::exit::CLEAN)
}
