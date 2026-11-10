//! `bohr ablate`: which faults a finding needed, and how real they are.
//!
//! The finding's run is repeated once per kind of fault that landed, with that kind's odds at
//! nought (`faults.ablate`). Every draw still happens, so the tape and every other fault stay where
//! they were. A kind whose removal makes the failure go, or turn into another, is necessary; the
//! finding's grade is the weakest grade among its necessary kinds.

use bohr_findings::ablation::{Ablation, Trial, Without, landed, scheduled_that_fired};
use bohr_tape::{TAPE_ABI, Tape};
use bohr_trace::{Bundle, EventKind, FaultGrade, FaultKind, PacketFate, SectionId};

use crate::{CliError, execute, replay};

/// The configuration with `kinds` added to `faults.ablate`.
fn without(config: &toml::Table, kinds: &[FaultKind]) -> Result<String, CliError> {
    let mut config = config.clone();
    let faults = config
        .entry("faults")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| CliError::usage("BOHR-4002: the recorded faults are not a table"))?;
    let ablate = faults
        .entry("ablate")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| CliError::usage("BOHR-4002: faults.ablate is not a list"))?;
    ablate.extend(kinds.iter().map(|kind| toml::Value::String(kind.name().to_owned())));
    toml::to_string(&config).map_err(|e| CliError::internal(e.to_string()))
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
    let loaded = crate::state_dir::load(&text)?;
    let target = replay::target_from(bundle, manifest, Some(&loaded), binaries)?;
    let mut run_config = crate::run_config_for(Some(&loaded), None);
    run_config.max_events = replay::budget_for(manifest, run_config.max_events);
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
        .and_then(|spooled| spooled.events_where(bohr_findings::group::telling).ok())
        .map(|(events, _)| events)
        .unwrap_or_default();
    let found = bohr_findings::group::finding_key(
        &completed.report.outcome,
        &events,
        &completed.output_tails,
    );
    Ok(if found == signature { Without::Same } else { Without::Other(found) })
}

/// Whether an event is one ablation reads: those a signature is read from, and every packet a
/// draw dropped, duplicated, reordered or corrupted.
fn ablation_reads(event: &bohr_trace::Event) -> bool {
    bohr_findings::group::telling(event)
        || matches!(event.kind, EventKind::PacketFate { fate, .. } if fate != PacketFate::Delivered)
}

/// Ablates the finding in `bytes`, looking for moved binaries under `binaries`.
pub(crate) fn ablation(bytes: &[u8], binaries: Option<&str>) -> Result<Ablation, CliError> {
    let bundle = Bundle::parse(bytes).map_err(|e| CliError::usage(e.to_string()))?;
    ablation_of(&bundle, binaries)
}

/// Ablates a parsed finding, looking for moved binaries under `binaries`.
pub(crate) fn ablation_of(bundle: &Bundle, binaries: Option<&str>) -> Result<Ablation, CliError> {
    let manifest = bundle.manifest().map_err(|e| CliError::usage(e.to_string()))?;
    // A clean run has nothing a fault could be needed for.
    if matches!(manifest.outcome, bohr_trace::Outcome::Clean) {
        return Err(CliError::usage(
            "BOHR-5003: this bundle records nothing going wrong, so there is nothing to ablate"
                .to_owned(),
        ));
    }
    manifest.check_tape_abi(TAPE_ABI).map_err(|e| CliError::usage(e.to_string()))?;
    manifest.require_single_execution().map_err(|e| CliError::usage(e.to_string()))?;
    replay::verify_binaries(bundle, &manifest, binaries)?;

    let tape_bytes = bundle
        .section(SectionId::Tape)
        .ok_or_else(|| CliError::usage("BOHR-4002: bundle is missing required section 2"))?;
    let tape = Tape::from_container(tape_bytes).map_err(|e| CliError::usage(e.to_string()))?;
    let events: Vec<bohr_trace::Event> = bundle
        .event_log()
        .map_err(|e| CliError::usage(e.to_string()))?
        .map(|raw| bohr_trace::read_events_where(raw, ablation_reads).0)
        .unwrap_or_default();
    let tails = bundle.output_tails().map_err(|e| CliError::usage(e.to_string()))?;
    let signature = bohr_findings::group::finding_key(&manifest.outcome, &events, &tails);

    let config_text = bundle
        .config_text()
        .map_err(|e| CliError::usage(format!("BOHR-4002: config section: {e}")))?;
    let config: toml::Table = config_text
        .parse()
        .map_err(|e| CliError::usage(format!("BOHR-4002: the recorded configuration: {e}")))?;
    // The same checks a replay makes: a run under another model, oracle or state would differ in
    // every trial, and every fault would read as needed.
    let loaded = if config_text.trim().is_empty() {
        None
    } else {
        Some(crate::state_dir::load(config_text)?)
    };
    replay::verify_state(&manifest, loaded.as_ref())?;
    let (world, oracle) = crate::identities(loaded.as_ref());
    manifest.check_provenance(&world, &oracle).map_err(|e| CliError::usage(e.to_string()))?;
    // A control with nothing suppressed must fail as the bundle did; one that does not is a run
    // this build does not reproduce, and no trial against it could tell what it needed.
    let control = trial(bundle, &manifest, &tape, &config, &[], binaries, &signature)?;
    if control != Without::Same {
        return Err(CliError::internal(format!(
            "BOHR-2011: the finding did not run to itself under this build, so what it needed \
             cannot be told: run again with nothing suppressed it came to {}",
            match control {
                Without::Clean => "a clean run".to_owned(),
                Without::Other(other) => other,
                Without::Same => String::new(),
            }
        )));
    }
    let fired = landed(&events);
    let scheduled = scheduled_that_fired(loaded.as_ref(), &fired);

    let drawn: Vec<FaultKind> = fired.into_iter().filter(|k| !scheduled.contains(k)).collect();
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
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
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
