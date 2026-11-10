//! What a program asks for that this build cannot answer.
//!
//! A run stops at the first unmodelled syscall (§5.7), so learning a program's surface that way
//! takes a run per call. A survey reports the whole set in one run.
//!
//! A survey is never a run: its worlds have their own identity, so a bundle claiming one is
//! refused; nothing is written to disk; and a survey that found anything exits non-zero. It is a
//! separate command, with no flag on `bohr run`.

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
            .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {path}: {e}")))?,
        None => String::new(),
    };
    let mut loaded = match config_path {
        Some(_) => Some(crate::state_dir::load(&config_toml)?),
        None => None,
    };
    // A dynamic target's libraries are trapped like its binary, as in a run.
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
    if crate::json::on() {
        let verdict = bohr_report::narrate::verdict(&completed.report.outcome);
        crate::json::emit(
            "survey",
            account(
                seed,
                completed.counters.events,
                completed.report.outcome == Outcome::Clean,
                &verdict,
                &completed.surveyed,
            ),
        );
    } else {
        report(&completed, seed);
    }
    // Exit 4, since the result is about this build. An incomplete guest also
    // counts as something to report; see `report`.
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
    // The scenario's horizon and ending node apply as in a run.
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
    Err(CliError::usage(
        "BOHR-3005: this build has no interceptor, so there is no program to survey".to_owned(),
    ))
}

/// Prints the set, and what it is not evidence of.
///
/// A survey answers `ENOSYS` and the guest takes its fallback, so calls behind a refused one never
/// appear: a program refused `io_uring_setup` never calls `io_uring_enter`. The list is a lower
/// bound, and the output says so.
///
/// How the guest ended is printed too. A dynamic program whose loader finds no libraries exits 127
/// before its first instruction; its short list measures nothing.
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
        let verdict = bohr_report::narrate::verdict(&completed.report.outcome);
        println!("  {}    {}", verdict.headline, verdict.code);
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

/// A survey as the body of a `bohr.survey` object: the walls, and whether the guest ran far
/// enough for them to be its surface.
///
/// `completed` false means the walls are no measurement, as [`report`] says in words.
fn account(
    seed: u64,
    events: u64,
    completed: bool,
    verdict: &bohr_report::narrate::Verdict,
    surveyed: &[(u32, u64)],
) -> serde_json::Value {
    serde_json::json!({
        "seed": seed,
        "events": events,
        "completed": completed,
        "outcome": verdict.code,
        "headline": verdict.headline,
        "walls": walls_json(surveyed),
    })
}

/// The walls a survey met, one row each: number, name, calls and which kind of wall.
pub(crate) fn walls_json(surveyed: &[(u32, u64)]) -> Vec<serde_json::Value> {
    surveyed
        .iter()
        .map(|(nr, calls)| {
            let (name, wall) = describe(*nr);
            serde_json::json!({ "nr": nr, "name": name, "calls": calls, "wall": wall })
        })
        .collect()
}

/// A syscall's name, and which kind of wall the guest met at it.
///
/// *nothing models it* has no table row; a run would be refused. *answered ENOSYS* has a row whose
/// model deliberately refuses. The fixes differ: the first needs a row, the second a model.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A survey's object lists each wall and whether the guest ran to completion.
    #[test]
    fn a_survey_object_names_its_walls_and_whether_the_guest_ran() {
        let clean = bohr_report::narrate::verdict(&Outcome::Clean);
        let value = crate::json::object("survey", account(1, 900, true, &clean, &[(425, 2)]));
        assert_eq!(value["schema"], "bohr.survey/1");
        assert_eq!(value["seed"], 1);
        assert_eq!(value["events"], 900);
        assert_eq!(value["completed"], true);
        assert_eq!(value["walls"][0]["nr"], 425);
        assert_eq!(value["walls"][0]["calls"], 2);
        assert!(value["walls"][0]["wall"].is_string());
        assert_eq!(account(1, 0, true, &clean, &[])["walls"], serde_json::json!([]));
    }
}
