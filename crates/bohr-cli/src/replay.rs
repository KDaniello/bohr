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

/// Runs a bundle's tape and scenario under this build and writes what they reach to `out`, whatever
/// model the bundle was recorded under: how a bank is carried to a new model.
///
/// The programs and the prepared state are checked as a replay checks them. The run is not compared
/// with the old one, whose model may answer differently; the caller reads what it reached.
///
/// # Errors
///
/// Every refusal a replay makes before running, and anything the run could not do.
pub(crate) fn record_again(
    bytes: &[u8],
    binaries: Option<&str>,
    out: &std::path::Path,
) -> Result<execute::Completed, CliError> {
    let bundle = Bundle::parse(bytes).map_err(|e| CliError::usage(e.to_string()))?;
    let manifest = bundle.manifest().map_err(|e| CliError::usage(e.to_string()))?;
    manifest.check_tape_abi(TAPE_ABI).map_err(|e| CliError::usage(e.to_string()))?;
    manifest.require_single_execution().map_err(|e| CliError::usage(e.to_string()))?;
    let tape_bytes = bundle
        .section(SectionId::Tape)
        .ok_or_else(|| CliError::usage("BOHR-4002: bundle is missing required section 2"))?;
    let tape = Tape::from_container(tape_bytes).map_err(|e| CliError::usage(e.to_string()))?;
    let config_text = bundle
        .config_text()
        .map_err(|e| CliError::usage(format!("BOHR-4002: config section: {e}")))?;
    let loaded = if config_text.trim().is_empty() {
        None
    } else {
        Some(crate::state_dir::load(config_text)?)
    };
    verify_binaries(&bundle, &manifest, binaries)?;
    verify_state(&manifest, loaded.as_ref())?;
    let target = target_from(&bundle, &manifest, loaded.as_ref(), binaries)?;
    let completed = execute::run_with(execute::Execution {
        target: &target,
        tape: &tape,
        // A run that ended on its event budget ends there again.
        config: {
            let mut config = crate::run_config_for(loaded.as_ref(), None);
            config.max_events = budget_for(&manifest, config.max_events);
            config
        },
        output: execute::Output::Discard,
        policy: &manifest.policy.scheduler,
        record: execute::Record::Spool,
        scenario: loaded.as_ref(),
        config_toml: config_text,
    })?;
    execute::write_bundle(&completed, &tape, None, out)?;
    Ok(completed)
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
    let bundle = Bundle::parse(bytes).map_err(|e| CliError::usage(e.to_string()))?;
    let manifest = bundle.manifest().map_err(|e| CliError::usage(e.to_string()))?;

    // Every refusal happens before anything runs.
    manifest.check_tape_abi(TAPE_ABI).map_err(|e| CliError::usage(e.to_string()))?;
    manifest.require_single_execution().map_err(|e| CliError::usage(e.to_string()))?;

    let tape_bytes = bundle
        .section(SectionId::Tape)
        .ok_or_else(|| CliError::usage("BOHR-4002: bundle is missing required section 2"))?;
    let tape = Tape::from_container(tape_bytes).map_err(|e| CliError::usage(e.to_string()))?;

    let config_text = bundle
        .config_text()
        .map_err(|e| CliError::usage(format!("BOHR-4002: config section: {e}")))?;
    let loaded = if config_text.trim().is_empty() {
        None
    } else {
        Some(crate::state_dir::load(config_text)?)
    };

    verify_binaries(&bundle, &manifest, binaries)?;
    verify_state(&manifest, loaded.as_ref())?;
    let target = target_from(&bundle, &manifest, loaded.as_ref(), binaries)?;

    let (world, oracle) = crate::identities(loaded.as_ref());
    manifest.check_provenance(&world, &oracle).map_err(|e| CliError::usage(e.to_string()))?;

    if let Some(at) = break_at {
        if at > manifest.counters.events {
            return Err(CliError::usage(format!(
                "BOHR-1002: this bundle recorded {} events; event {at} cannot be reached",
                manifest.counters.events
            )));
        }
        crate::handover::refuse_if_there_is_no_process(&target)?;
        crate::handover::refuse_if_a_debugger_cannot_attach()?;
    }

    // The bundle's configuration sets the bounds, as it sets the world and the oracle.
    let mut run_config = crate::run_config_for(loaded.as_ref(), None);
    run_config.max_events = budget_for(&manifest, run_config.max_events);
    run_config.break_at = break_at.map(bohr_types::EventId);
    // Only digested: a replay that matches needs no log, and one that diverges is run again
    // with its log kept, to the same events, to name where.
    let exec = execute::Execution {
        target: &target,
        tape: &tape,
        config: run_config,
        output: execute::Output::Discard,
        policy: &manifest.policy.scheduler,
        record: execute::Record::DigestOnly,
        scenario: loaded.as_ref(),
        config_toml: config_text,
    };
    if break_at.is_some() {
        return execute::hand_over_at(exec).map(|()| Verdict::HandedOver);
    }
    let completed = execute::once(exec)?;

    if completed.report.digest.to_string() != manifest.trace_digest {
        let logged = execute::once(execute::Execution { record: execute::Record::Log, ..exec })?;
        return Ok(Verdict::Diverged(diverged(&bundle, &logged.log, &manifest)));
    }

    // Checked even when the digest matched: a journal that no longer regenerates means the trace
    // matched for another reason.
    if let Some(stored) = bundle.section(SectionId::DrawJournal) {
        let regenerated = completed.report.journal.to_bytes();
        if stored != regenerated.as_slice() {
            return Err(CliError::internal(format!(
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
        .and_then(|one| one.events().ok().flatten())
        .and_then(|b| read_events(b).ok());
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

/// Refuses a run again from a prepared state other than the recorded one, or from one the bundle
/// recorded no digest of (`BOHR-4004`).
pub(crate) fn verify_state(
    manifest: &bohr_trace::Manifest,
    loaded: Option<&bohr_world::LoadedConfig>,
) -> Result<(), CliError> {
    let image = loaded.and_then(|l| l.state.as_ref());
    let (recorded, image) = match (manifest.state_digest.as_deref(), image) {
        (None, None) => return Ok(()),
        (None, Some(_)) => {
            return Err(CliError::usage(
                "BOHR-4004: the configuration names a prepared state and the bundle recorded no \
                 digest of one, so the state the run began from is unknown"
                    .to_owned(),
            ));
        }
        (Some(recorded), None) => {
            return Err(CliError::usage(format!(
                "BOHR-4004: this run began from a prepared state ({recorded}) and the \
                 configuration names none"
            )));
        }
        (Some(recorded), Some(image)) => (recorded, image),
    };

    let actual = format!("blake3:{}", image.digest().to_hex());
    if actual != recorded {
        return Err(CliError::usage(format!(
            "BOHR-4004: this run began from {recorded} and the configuration names {actual}"
        )));
    }
    Ok(())
}

/// The event budget a stored run is run again under: the one it recorded.
///
/// A bundle from before budgets were recorded is held to the events it recorded where the budget
/// ended it, so it ends at the same event; otherwise the budget never cuts it short.
pub(crate) fn budget_for(manifest: &Manifest, default: u64) -> u64 {
    budget(manifest.max_events, &manifest.outcome, manifest.counters.events, default)
}

/// [`budget_for`], from the three things a manifest says about it.
fn budget(recorded: Option<u64>, outcome: &bohr_trace::Outcome, events: u64, default: u64) -> u64 {
    if let Some(budget) = recorded {
        return budget;
    }
    let ended_on_budget = matches!(
        outcome,
        bohr_trace::Outcome::GuestHung(said) if said.starts_with("event budget reached")
    );
    if ended_on_budget { events } else { default.max(events + 1) }
}

/// Checks every recorded object still hashes to what the bundle says.
///
/// A changed binary is the most common way a replay turns into a different run, so it is refused
/// (`BOHR-4004`). See `ARCHITECTURE.md` §5.9.
pub(crate) fn verify_binaries(
    bundle: &Bundle,
    manifest: &Manifest,
    dir: Option<&str>,
) -> Result<(), CliError> {
    let records = recorded_binaries(bundle)?;

    for record in records {
        // A built-in guest is this binary; there is no separate object whose bytes could drift.
        // Only the null backend runs one: to a backend that executes files, the name is a path.
        if record.path.starts_with("builtin:") {
            if manifest.provenance.backend != "null" {
                return Err(CliError::usage(format!(
                    "BOHR-4004: {} is a built-in guest in a bundle the {} backend wrote, so it is \
                     not run",
                    record.path, manifest.provenance.backend
                )));
            }
            continue;
        }
        // The probe is found as a run finds it and checked against the recorded build.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        let path = match crate::probe::supplied(&record.path) {
            Some(found) if record.role == bohr_trace::BinaryRole::Probe => found?,
            _ => relocated(&record.path, dir),
        };
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        let path = relocated(&record.path, dir);
        // A bundle names the path: only a regular file of a program's size is read, so a device
        // that never ends or a pipe that never answers is refused instead of read.
        const MAX_PROGRAM_BYTES: u64 = 4 << 30;
        let meta = std::fs::metadata(&path).map_err(|e| {
            CliError::usage(format!("BOHR-4004: cannot read {path} to check it: {e}"))
        })?;
        if !meta.is_file() || meta.len() > MAX_PROGRAM_BYTES {
            return Err(CliError::usage(format!(
                "BOHR-4004: {path} is not a regular file of a program's size, so it is not read"
            )));
        }
        let bytes = std::fs::read(&path).map_err(|e| {
            CliError::usage(format!("BOHR-4004: cannot read {path} to check it: {e}"))
        })?;
        let actual = Digest::of(&bytes).to_string();
        // A run of `--patch-cpuid` recorded the trapped copy's digest under the program's name:
        // the program is trapped again, and the copy runs in its place.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if actual != record.blake3
            && let Some((patched, _)) = bohr_x86::cpuid::trapped(&bytes)
            && Digest::of(&patched).to_string() == record.blake3
        {
            let copy = crate::private::place("trapped", &patched)?;
            crate::private::substitute(&record.path, &copy);
            continue;
        }
        if actual != record.blake3 {
            // The probe ships with bohr, so every release changes it; the old one is named apart.
            let remedy = if record.role == bohr_trace::BinaryRole::Probe {
                "\n  the probe is the one this bohr found; name the probe the bundle was written \
                 with in BOHR_PROBE"
            } else {
                ""
            };
            return Err(CliError::usage(format!(
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
            CliError::usage(format!("BOHR-4002: this build has no guest named {name}"))
        })?;
        return Ok(execute::Target::Demo(name, demo.program()));
    }
    program_target(bundle, loaded, binaries)
}

/// The objects a bundle recorded, or `BOHR-4002` for a section that does not read.
fn recorded_binaries(bundle: &Bundle) -> Result<Vec<bohr_trace::BinaryRecord>, CliError> {
    bundle
        .binaries()
        .map_err(|e| CliError::usage(format!("BOHR-4002: binaries section is not readable: {e}")))
}

/// The guest a built-in bundle names, read from the object it recorded.
fn builtin_name(bundle: &Bundle) -> Result<String, CliError> {
    recorded_binaries(bundle)?
        .first()
        .map(|r| r.path.trim_start_matches("builtin:").to_owned())
        .ok_or_else(|| CliError::usage("BOHR-4002: bundle names no guest to replay"))
}

/// Rebuilds a traced target from the recorded configuration and objects.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn program_target(
    bundle: &Bundle,
    loaded: Option<&bohr_world::LoadedConfig>,
    binaries: Option<&str>,
) -> Result<execute::Target, CliError> {
    let records = recorded_binaries(bundle)?;
    let first = records
        .first()
        .ok_or_else(|| CliError::usage("BOHR-4002: bundle names no program to replay"))?;

    // The recorded argument list, unchanged even when the files moved: it shapes the stack and so
    // the digest. A moved file is executed in place of `argv[0]`, the same file
    // `verify_binaries` checked.
    let mut argv = vec![first.path.clone()];
    argv.extend(first.args.iter().cloned());
    let mut specs = crate::specs_for(argv, loaded, None)?;
    if let Some(program) =
        unrecorded(specs.iter().filter_map(|s| s.argv.first().map(String::as_str)), &records)
    {
        return Err(CliError::usage(format!(
            "BOHR-4004: the configuration starts {program}, and the bundle holds no digest of it, \
             so it is not run"
        )));
    }
    if binaries.is_some() {
        for spec in specs.iter_mut().filter(|s| s.executable.is_none()) {
            if let Some(program) = spec.argv.first() {
                // The backend pads every executed path to one length and shows the guest its
                // recorded name, so a moved file runs where it is.
                spec.executable = Some(relocated(program, binaries));
            }
        }
    }
    Ok(execute::Target::Program(specs))
}

/// The first program a run would start that the bundle holds no digest of.
///
/// A replay starts what its configuration names, so each of those needs a record that
/// [`verify_binaries`] checked; a record without a digest checks nothing.
#[cfg(any(test, all(target_os = "linux", target_arch = "x86_64")))]
fn unrecorded<'a>(
    programs: impl IntoIterator<Item = &'a str>,
    records: &[bohr_trace::BinaryRecord],
) -> Option<&'a str> {
    programs
        .into_iter()
        .find(|program| !records.iter().any(|r| r.path == *program && !r.blake3.is_empty()))
}

/// The same, where nothing can trace a program.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn program_target(
    _bundle: &Bundle,
    _loaded: Option<&bohr_world::LoadedConfig>,
    _binaries: Option<&str>,
) -> Result<execute::Target, CliError> {
    Err(CliError::usage(
        "BOHR-1001: this bundle was produced under interception, and this build has no syscall \
         backend to replay it with"
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use bohr_trace::{BinaryRecord, Bundle, Outcome};

    use super::{budget, unrecorded, verify_binaries, verify_state};

    const BUILTIN: &[u8] = include_bytes!("../../../testdata/bundles/two-tasks-seed0.bohr");

    fn record(path: &str, blake3: &str) -> BinaryRecord {
        BinaryRecord {
            path: path.to_owned(),
            blake3: blake3.to_owned(),
            role: bohr_trace::BinaryRole::Executable,
            args: Vec::new(),
        }
    }

    /// A program the configuration starts is run only when the bundle holds its digest.
    #[test]
    fn a_program_the_bundle_never_hashed_is_found() {
        let records = [record("/bin/server", "blake3:00"), record("builtin:evil", "")];
        assert_eq!(unrecorded(["/bin/server"], &records), None);
        assert_eq!(unrecorded(["/bin/server", "/bin/other"], &records), Some("/bin/other"));
        assert_eq!(unrecorded(["builtin:evil"], &records), Some("builtin:evil"), "no digest");
    }

    /// A built-in record is skipped only in a bundle the null backend wrote.
    #[test]
    fn a_builtin_record_is_trusted_only_from_the_null_backend() {
        let bundle = Bundle::parse(BUILTIN).expect("golden bundle");
        let mut manifest = bundle.manifest().expect("manifest");
        assert!(verify_binaries(&bundle, &manifest, None).is_ok());
        manifest.provenance.backend = "ptrace".to_owned();
        let refused = verify_binaries(&bundle, &manifest, None).err().map(|e| e.to_string());
        assert!(refused.is_some_and(|e| e.contains("BOHR-4004")), "a builtin: path ran unchecked");
    }

    /// A configuration naming a prepared state needs the digest the run recorded of it.
    #[test]
    fn a_state_without_a_recorded_digest_is_refused() {
        let dir = std::env::temp_dir().join(format!("bohr-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("state dir");
        std::fs::write(dir.join("file"), b"contents").expect("state file");
        let text = format!("[state]\ndir = '{}'\n", dir.display());
        let loaded = crate::state_dir::load(&text).expect("config");
        let _ = std::fs::remove_dir_all(&dir);

        let mut manifest = Bundle::parse(BUILTIN).and_then(|b| b.manifest()).expect("manifest");
        assert!(manifest.state_digest.is_none());
        let refused = verify_state(&manifest, Some(&loaded)).err().map(|e| e.to_string());
        assert!(refused.is_some_and(|e| e.contains("no digest")), "an unrecorded state loaded");

        let image = loaded.state.as_ref().expect("image");
        manifest.state_digest = Some(format!("blake3:{}", image.digest().to_hex()));
        assert!(verify_state(&manifest, Some(&loaded)).is_ok(), "the recorded state is refused");
    }

    /// A run the budget ended is run again to the same event, never one past it.
    #[test]
    fn a_run_the_budget_ended_ends_at_the_same_event_again() {
        let hung = Outcome::GuestHung("event budget reached at 7".to_owned());
        assert_eq!(budget(Some(5), &hung, 5, 10_000_000), 5, "the recorded budget is kept");
        assert_eq!(budget(None, &hung, 7, 10_000_000), 7, "an older bundle stops where it stopped");
        let clean = Outcome::Clean;
        assert_eq!(budget(None, &clean, 7, 10_000_000), 10_000_000, "the budget never cuts it");
        assert_eq!(budget(None, &clean, 20_000_000, 10_000_000), 20_000_001);
    }
}
