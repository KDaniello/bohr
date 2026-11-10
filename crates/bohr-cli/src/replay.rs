//! Running a stored bundle again and checking it produced what it says it did.
//!
//! A replay is an ordinary run driven by the bundle's tape; it succeeds when the digest matches.
//! A foreign world model, another draw ABI or a changed binary is refused, so a replay never runs
//! something else. See `docs/API.md` §7.3.

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
    /// Verifies nothing: the run stops early, so there is no digest to compare.
    HandedOver,
}

/// Replays one bundle, stopping at `break_at` and handing the world to a debugger if given.
///
/// `binaries` is where to look for recorded objects whose paths moved; a missing or changed object
/// is refused.
pub(crate) fn replay_to(
    bytes: &[u8],
    binaries: Option<&str>,
    break_at: Option<u64>,
) -> Result<Verdict, CliError> {
    let bundle = Bundle::parse(bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    let manifest = bundle.manifest().map_err(|e| CliError::Usage(e.to_string()))?;

    // Every refusal happens before anything runs.
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

    // The bundle's configuration sets the bounds, as it sets the world and the oracle.
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

    // Checked even when the digest matched: a journal that no longer regenerates means the trace
    // matched for another reason.
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
/// The first differing event is the diagnosis. An undecodable stored log is reported as such.
fn diverged(bundle: &Bundle, log: &[u8], manifest: &Manifest) -> String {
    // A bundle that is not one execution gives nothing to compare against.
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
        Some(e) => bohr_report::text::event_line(e).trim_end().to_owned(),
        None => "ended before this event".to_owned(),
    };
    format!(
        "BOHR-2011: replay diverged at event {index}\n  recorded: {}\n  replayed: {}",
        describe(recorded.get(index)),
        describe(actual.get(index))
    )
}

/// Refuses a replay whose prepared state differs from the one the run began from (`BOHR-4004`).
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

/// Checks every recorded object still hashes to what the bundle says.
///
/// A changed binary is the most common way a replay turns into a different run, so it is refused
/// (`BOHR-4004`). See `ARCHITECTURE.md` §5.9.
pub(crate) fn verify_binaries(bundle: &Bundle, dir: Option<&str>) -> Result<(), CliError> {
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
        // The probe is found as a run finds it and checked against the recorded build.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        let path = if record.role == "probe" {
            crate::probe::program()?
        } else {
            relocated(&record.path, dir)
        };
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        let path = relocated(&record.path, dir);
        let bytes = std::fs::read(&path).map_err(|e| {
            CliError::Usage(format!("BOHR-4004: cannot read {path} to check it: {e}"))
        })?;
        let actual = Digest::of(&bytes).to_string();
        if actual != record.blake3 {
            // The probe ships with bohr, so every release changes it; the old one is named apart.
            let remedy = if record.role == "probe" {
                "\n  the probe is the one this bohr found; name the probe the bundle was written \
                 with in BOHR_PROBE"
            } else {
                ""
            };
            return Err(CliError::Usage(format!(
                "BOHR-4004: {path} has changed since the bundle was written\n  \
                 recorded {}\n  found    {actual}{remedy}",
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

/// The moved file's path, padded with slashes to the recorded path's length.
///
/// The kernel copies the executed path onto the new stack (`AT_EXECFN`), so its length moves every
/// address below it, and with them the digest. Repeated slashes name the same file; a longer path
/// cannot be shortened and is refused before the run.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn executed_as(path: &str, dir: Option<&str>) -> Result<String, CliError> {
    let moved = relocated(path, dir);
    if moved.len() > path.len() {
        return Err(CliError::Usage(format!(
            "BOHR-4004: {moved} is longer than the recorded {path}, which would move the guest's \
             stack; name the --binaries directory by a shorter path, such as a relative one"
        )));
    }
    let Some((parent, name)) = moved.rsplit_once('/') else {
        return Ok(moved);
    };
    let pad = path.len() - moved.len();
    Ok(format!("{parent}/{}{name}", "/".repeat(pad)))
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

    // The recorded argument list, unchanged even when the files moved: it shapes the stack and so
    // the digest. A moved file is executed in place of `argv[0]`, the same file
    // `verify_binaries` checked.
    let mut argv = vec![first.path.clone()];
    argv.extend(first.args.iter().cloned());
    let mut specs = crate::specs_for(argv, loaded, None)?;
    if binaries.is_some() {
        for spec in specs.iter_mut().filter(|s| s.executable.is_none()) {
            if let Some(program) = spec.argv.first() {
                spec.executable = Some(executed_as(program, binaries)?);
            }
        }
    }
    Ok(execute::Target::Program(specs))
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
