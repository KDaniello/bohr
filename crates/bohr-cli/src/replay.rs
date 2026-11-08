//! Running a stored bundle again and checking it produced what it says it did.
//!
//! There are not two engines: a replay is an ordinary run driven by the bundle's own tape, and
//! success is the digest coming back the same. Everything a replay refuses — a foreign world
//! model, another draw ABI, a binary that has changed underneath it — is refused rather than
//! worked around, because a replay that quietly ran something else would be worse than none.
//! See `docs/API.md` §7.3.

use bohr_tape::{TAPE_ABI, Tape};
use bohr_trace::{Bundle, Manifest, SectionId, read_events};
use bohr_types::Digest;

use crate::{CliError, execute};

/// What a replay concluded.
pub(crate) enum Verdict {
    /// The digest came back the same.
    Matched(Box<Manifest>),
    /// The run diverged from what the bundle recorded.
    Diverged(String),
    /// The run was stopped where it was asked to be and given to a debugger.
    ///
    /// A hand-over verifies nothing: the run never reaches its end, so there is no digest to
    /// compare and no journal to regenerate. Saying so is the point of the variant — a caller that
    /// treated this as a match would report a bundle as verified by a run that stopped early.
    HandedOver,
}

/// Replays one bundle.
///
/// `binaries` is where to look for the recorded objects when their paths have moved; without it
/// the recorded paths are used, and a missing or changed object is a refusal rather than a run
/// against whatever is there now.
/// Replays one bundle, stopping at an event where one was named and handing the world over there.
pub(crate) fn replay_to(
    bytes: &[u8],
    binaries: Option<&str>,
    break_at: Option<u64>,
) -> Result<Verdict, CliError> {
    let bundle = Bundle::parse(bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    let manifest = bundle.manifest().map_err(|e| CliError::Usage(e.to_string()))?;

    // Every refusal happens before anything runs: a check made afterwards has already spent the
    // time it exists to save, and has a plausible wrong answer in hand to be tempted by.
    manifest.check_tape_abi(TAPE_ABI).map_err(|e| CliError::Usage(e.to_string()))?;
    manifest.require_single_execution().map_err(|e| CliError::Usage(e.to_string()))?;

    let tape_bytes = bundle
        .section(SectionId::Tape)
        .ok_or_else(|| CliError::Usage("BOHR-4002: bundle is missing required section 2".into()))?;
    let tape = Tape::from_container(tape_bytes).map_err(|e| CliError::Usage(e.to_string()))?;

    let config_text = bundle.section(SectionId::Config).unwrap_or_default();
    let config_text = std::str::from_utf8(config_text)
        .map_err(|e| CliError::Usage(format!("BOHR-4002: config section is not text: {e}")))?;
    let loaded = if config_text.trim().is_empty() {
        None
    } else {
        Some(bohr_world::config::load(config_text)?)
    };

    verify_binaries(&bundle, binaries)?;
    verify_state(&manifest, loaded.as_ref())?;
    let target = target_from(&bundle, &manifest, loaded.as_ref(), binaries)?;

    let (world, oracle) = crate::identities(loaded.as_ref());
    manifest.check_provenance(&world, &oracle).map_err(|e| CliError::Usage(e.to_string()))?;

    if let Some(at) = break_at {
        if at > manifest.counters.events {
            return Err(CliError::Usage(format!(
                "BOHR-1002: this bundle recorded {} events; event {at} cannot be reached",
                manifest.counters.events
            )));
        }
        crate::handover::refuse_if_there_is_no_process(&target)?;
        crate::handover::refuse_if_a_debugger_cannot_attach()?;
    }

    // The bundle's own configuration decides the bounds, exactly as it decides the world and the
    // oracle: a run recorded to a horizon and replayed without one runs past where it ended and
    // diverges, which would be reported as a defect in the tool rather than as the bound nobody
    // carried over.
    let mut run_config = crate::run_config_for(loaded.as_ref(), None);
    run_config.max_events = run_config.max_events.max(manifest.counters.events + 1);
    run_config.break_at = break_at.map(bohr_types::EventId);
    let exec = execute::Execution {
        target: &target,
        tape: &tape,
        config: run_config,
        output: execute::Output::Discard,
        policy: &manifest.policy.scheduler,
        record: execute::Record::Log,
        scenario: loaded.as_ref(),
        config_toml: config_text,
    };
    if break_at.is_some() {
        return execute::hand_over_at(exec).map(|()| Verdict::HandedOver);
    }
    let completed = execute::once(exec)?;

    if completed.report.digest.to_string() != manifest.trace_digest {
        return Ok(Verdict::Diverged(diverged(&bundle, &completed.log, &manifest)));
    }

    // Checked even where the digest matched, which is the whole point of storing it: the journal
    // says where each decision came from, and a journal that no longer regenerates means the
    // trace matched for a reason other than the one recorded.
    if let Some(stored) = bundle.section(SectionId::DrawJournal) {
        let regenerated = completed.report.journal.to_bytes();
        if stored != regenerated.as_slice() {
            return Err(CliError::Internal(format!(
                "BOHR-2010: the draw journal regenerated differently ({} bytes stored, {} now)",
                stored.len(),
                regenerated.len()
            )));
        }
    }

    Ok(Verdict::Matched(Box::new(manifest)))
}

/// Names the first event where the replay and the bundle disagree.
///
/// "Digests differ" leaves the reader with nowhere to go; the first differing event is the whole
/// diagnosis, and a replay that could not decode the stored log says so rather than guessing.
fn diverged(bundle: &Bundle, log: &[u8], manifest: &Manifest) -> String {
    // A divergence report is describing a run that already happened, so a bundle it cannot read as
    // one execution leaves it with nothing to compare against rather than with a reason to fail.
    let recorded = bundle
        .single()
        .ok()
        .and_then(|one| one.events().map(<[u8]>::to_vec))
        .and_then(|b| read_events(&b).ok());
    let (Some(recorded), Ok(actual)) = (recorded, read_events(log)) else {
        return format!(
            "BOHR-2011: replay produced a different digest, and the stored log could not be read\n  \
             recorded {}",
            manifest.trace_digest
        );
    };
    let index = (0..recorded.len().max(actual.len()))
        .find(|i| recorded.get(*i) != actual.get(*i))
        .unwrap_or(0);
    let describe = |e: Option<&bohr_trace::Event>| match e {
        Some(e) => format!("{:?}", e.kind),
        None => "ended before this event".to_owned(),
    };
    format!(
        "BOHR-2011: replay diverged at event {index}\n  recorded: {}\n  replayed: {}",
        describe(recorded.get(index)),
        describe(actual.get(index))
    )
}

/// Checks every recorded object still hashes to what the bundle says.
///
/// A binary that changed underneath a bundle is the most common way a replay silently becomes a
/// different run, so this is a refusal (`BOHR-4004`) and never a warning. See `ARCHITECTURE.md`
/// §5.9. Refuses a replay whose prepared state is not the one the run began from.
///
/// The same shape as the binary fingerprint check and for the same reason: a run that started from
/// a different filesystem is a different run, and letting it proceed produces a divergence at the
/// first read that gets reported as a defect in the tool.
fn verify_state(
    manifest: &bohr_trace::Manifest,
    loaded: Option<&bohr_world::LoadedConfig>,
) -> Result<(), CliError> {
    let Some(recorded) = manifest.state_digest.as_deref() else {
        return Ok(());
    };
    let Some(image) = loaded.and_then(|l| l.state.as_ref()) else {
        return Err(CliError::Usage(format!(
            "BOHR-4004: this run began from a prepared state ({recorded}) and the configuration \
             names none"
        )));
    };

    let actual = format!("blake3:{}", image.digest().to_hex());
    if actual != recorded {
        return Err(CliError::Usage(format!(
            "BOHR-4004: this run began from {recorded} and the configuration names {actual}"
        )));
    }
    Ok(())
}

fn verify_binaries(bundle: &Bundle, dir: Option<&str>) -> Result<(), CliError> {
    let Some(raw) = bundle.section(SectionId::Binaries) else {
        return Ok(());
    };
    let records: Vec<bohr_trace::BinaryRecord> = serde_json::from_slice(raw).map_err(|e| {
        CliError::Usage(format!("BOHR-4002: binaries section is not readable: {e}"))
    })?;

    for record in records {
        // A built-in guest is this binary; there is no separate object whose bytes could drift.
        if record.path.starts_with("builtin:") {
            continue;
        }
        let path = relocated(&record.path, dir);
        let bytes = std::fs::read(&path).map_err(|e| {
            CliError::Usage(format!("BOHR-4004: cannot read {path} to check it: {e}"))
        })?;
        let actual = Digest::of(&bytes).to_string();
        if actual != record.blake3 {
            return Err(CliError::Usage(format!(
                "BOHR-4004: {path} has changed since the bundle was written\n  \
                 recorded {}\n  found    {actual}",
                record.blake3
            )));
        }
    }
    Ok(())
}

/// Where an object is looked for: under `--binaries` by base name, or where it was recorded.
fn relocated(path: &str, dir: Option<&str>) -> String {
    match dir {
        Some(dir) => {
            let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
            format!("{}/{name}", dir.trim_end_matches(['/', '\\']))
        }
        None => path.to_owned(),
    }
}

/// Reconstructs what to run from what the bundle recorded.
///
/// A bundle from a built-in guest names it, so it replays on any operating system; one from a
/// real program names its argv, and replaying it needs a host that can trace.
pub(crate) fn target_from(
    bundle: &Bundle,
    manifest: &Manifest,
    loaded: Option<&bohr_world::LoadedConfig>,
    binaries: Option<&str>,
) -> Result<execute::Target, CliError> {
    if manifest.provenance.backend == "null" {
        let name = builtin_name(bundle)?;
        let demo = crate::guests::Demo::named(&name).ok_or_else(|| {
            CliError::Usage(format!("BOHR-4002: this build has no guest named {name}"))
        })?;
        return Ok(execute::Target::Demo(name, demo.program()));
    }
    program_target(bundle, loaded, binaries)
}

/// The guest a built-in bundle names, read from the object it recorded.
fn builtin_name(bundle: &Bundle) -> Result<String, CliError> {
    let raw = bundle.section(SectionId::Binaries).unwrap_or_default();
    let records: Vec<bohr_trace::BinaryRecord> = serde_json::from_slice(raw).unwrap_or_default();
    records
        .first()
        .map(|r| r.path.trim_start_matches("builtin:").to_owned())
        .ok_or_else(|| CliError::Usage("BOHR-4002: bundle names no guest to replay".into()))
}

/// Rebuilds a traced target from the recorded configuration and objects.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn program_target(
    bundle: &Bundle,
    loaded: Option<&bohr_world::LoadedConfig>,
    binaries: Option<&str>,
) -> Result<execute::Target, CliError> {
    let raw = bundle.section(SectionId::Binaries).unwrap_or_default();
    let records: Vec<bohr_trace::BinaryRecord> = serde_json::from_slice(raw).unwrap_or_default();
    let first = records
        .first()
        .ok_or_else(|| CliError::Usage("BOHR-4002: bundle names no program to replay".into()))?;

    // The program and what it was started with. Only the program was taken, so a run saved with
    // arguments replayed without them -- a different stack, a different digest, and a bundle that
    // reported BOHR-2011 against the tool that wrote it.
    let mut argv = vec![relocated(&first.path, binaries)];
    argv.extend(first.args.iter().cloned());
    Ok(execute::Target::Program(crate::specs_for(argv, loaded, None)))
}

/// The same, where nothing can trace a program.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn program_target(
    _bundle: &Bundle,
    _loaded: Option<&bohr_world::LoadedConfig>,
    _binaries: Option<&str>,
) -> Result<execute::Target, CliError> {
    Err(CliError::Usage(
        "BOHR-1001: this bundle was produced under interception, and this build has no syscall \
         backend to replay it with"
            .to_owned(),
    ))
}
