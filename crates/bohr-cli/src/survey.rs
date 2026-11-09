//! What a program asks for that this build cannot answer.
//!
//! An ordinary run refuses the first unmodelled syscall and ends, which is §5.7 and is the
//! right thing for a run: a call that reached the host unnoticed is a hole nothing downstream would
//! find. It also means that learning a real program's surface one call at a time costs a run per
//! call, and `docs/ARCHITECTURE.md` §5.7 promises the opposite — the whole set, up front. This is
//! that promise.
//!
//! A survey is not a run and must never be mistaken for one, which is arranged rather than asked
//! for: the worlds it uses carry an identity of their own so a bundle claiming one is refused
//! before it replays, nothing is written to disk, and a survey that found anything exits non-zero.
//! There is no flag on `bohr run` that does this, and there must never be — the moment there is,
//! somebody's continuous integration has it switched on.

use bohr_tape::Tape;
use bohr_trace::Outcome;
use bohr_types::Seed;
use bohr_world::LoadedConfig;

use crate::{CliError, execute, exit};

/// Runs `program` and reports every syscall it asked for that nothing here models.
pub(crate) fn run(
    program: Vec<String>,
    seed: u64,
    config_path: Option<&str>,
    max_events: Option<u64>,
    patch_cpuid: bool,
) -> Result<u8, CliError> {
    let config_toml = match config_path {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?,
        None => String::new(),
    };
    let mut loaded = match config_path {
        Some(_) => Some(bohr_world::config::load(&config_toml)?),
        None => None,
    };
    // The libraries a dynamic target loads ask the processor too, so they are trapped as its own
    // binary is: a survey of a program prepared differently from its run surveys another program.
    if patch_cpuid
        && let Some(scenario) = loaded.as_mut()
        && let Some(state) = scenario.state.take()
    {
        let (patched, rewritten) = state.with_processor_questions_trapped();
        for (path, sites) in &rewritten {
            eprintln!("trapped {sites} processor question(s) in {path}");
        }
        scenario.state = Some(patched);
    }

    let completed = surface(
        program,
        seed,
        loaded.as_ref(),
        &config_toml,
        max_events,
        execute::Record::DigestOnly,
    )?;
    report(&completed, seed);
    // Never zero with something to report, and never one: a survey found nothing about the guest,
    // it found something about this build. A guest that did not run to completion is something to
    // report -- see `report` for why it cannot be allowed to read as a clean surface.
    let ran = completed.report.outcome == Outcome::Clean;
    Ok(if completed.surveyed.is_empty() && ran { exit::CLEAN } else { exit::DEGRADED })
}

/// One survey run: what the program asked for that nothing models, and how it ended.
pub(crate) fn surface(
    program: Vec<String>,
    seed: u64,
    loaded: Option<&LoadedConfig>,
    config_toml: &str,
    max_events: Option<u64>,
    record: execute::Record,
) -> Result<execute::Completed, CliError> {
    let tape = Tape::from_seed(Seed(seed));
    // The scenario's horizon and its node to end with reach a survey as they reach a run: a survey
    // that ignored them ran a different, longer program than the one it was asked about.
    let run_config = crate::run_config_for(loaded, max_events);
    let target = target_for(program, loaded)?;
    execute::survey_with(execute::Execution {
        target: &target,
        tape: &tape,
        config: run_config,
        output: execute::Output::Discard,
        policy: "random",
        record,
        scenario: loaded,
        config_toml,
    })
}

/// The programs to survey, one spec per node, where this host can intercept one at all.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn target_for(
    program: Vec<String>,
    loaded: Option<&LoadedConfig>,
) -> Result<execute::Target, CliError> {
    Ok(execute::Target::Program(crate::specs_for(program, loaded, None)?))
}

/// Refuses on a host with no interceptor, for the same reason `bohr run` does.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn target_for(
    _program: Vec<String>,
    _loaded: Option<&LoadedConfig>,
) -> Result<execute::Target, CliError> {
    Err(CliError::Usage(
        "BOHR-3005: this build has no interceptor, so there is no program to survey".to_owned(),
    ))
}

/// Prints the set, and what it is not evidence of.
///
/// The second half is not decoration. A survey answers `ENOSYS` and lets the guest carry on, so a
/// program refused `io_uring_setup` takes its fallback path and never issues `io_uring_enter` at
/// all — which means a survey naming a mechanism is reporting less than it appears to, and a survey
/// silent about one is reporting nothing about it either way. A reader who takes this list for the
/// program's whole surface will build against the wrong list.
///
/// How the guest ended is printed for the same reason, and is the sharper case. A dynamically
/// linked program whose loader finds none of its libraries asks a dozen calls, is answered
/// every one of them, and exits 127 without reaching its own first instruction — and "every
/// call it made was answered" is then true and worthless, indistinguishable from the survey of
/// a program that genuinely asks for nothing. The surface of a guest that did not run is not a
/// small surface; it is no measurement at all, and it says so here.
fn report(completed: &execute::Completed, seed: u64) {
    println!(
        "survey  seed {seed}  {} events  {}",
        completed.counters.events,
        if completed.surveyed.is_empty() {
            "every call it made was answered"
        } else {
            "walls below"
        }
    );

    if completed.report.outcome != Outcome::Clean {
        println!(
            "
the guest did not run to completion, so this is not its surface:"
        );
        println!("  {:?}", completed.report.outcome);
    }

    if completed.surveyed.is_empty() {
        return;
    }

    println!("\n   nr  name                    calls  wall");
    for (nr, calls) in &completed.surveyed {
        let (name, wall) = describe(*nr);
        println!("{nr:>5}  {name:<22}  {calls:>5}  {wall}");
    }

    println!(
        "\nthis is one path through one run at one seed, and it is a lower bound twice over: a\n\
         call answered ENOSYS sends the guest down its fallback path, which is not the path it\n\
         would have taken; and a call it never reached is not a call it cannot make."
    );
}

/// A syscall's name, and which kind of wall the guest met at it.
///
/// The two are not the same work and must not read as though they were. *nothing models it* is a
/// table entry that does not exist and a run that would have been refused; *answered ENOSYS* is a
/// table entry that does exist and a model that deliberately says it cannot, so the caller takes
/// its own fallback. Told apart here because the fix differs: the first wants a row, the second
/// wants a model.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn describe(nr: u32) -> (&'static str, &'static str) {
    if let Some(name) = bohr_backend_ptrace::unmodelled_syscall_name(nr) {
        return (name, "nothing models it");
    }
    match bohr_backend_ptrace::syscall_name(nr) {
        Some(name) => (name, "answered ENOSYS"),
        None => ("?", "nothing models it"),
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) fn describe(_nr: u32) -> (&'static str, &'static str) {
    ("?", "nothing models it")
}
