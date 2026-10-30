//! Driving a campaign, and the workers that carry it out.
//!
//! The search itself is in `bohr-explorer` and knows nothing about processes. What is here is the
//! plumbing: split a generation across workers, wait, hand the results back in one piece, and save
//! what was found. The division matters — every claim about reproducibility is decided in the
//! crate that has no processes in it, and this file cannot weaken one by being written badly. The
//! worst it can do is be slow, or lose a result and say so.
//!
//! **Workers are processes.** A guest wedged in a way that confuses the kernel's `ptrace`
//! bookkeeping must take one worker with it and not the campaign; and two traced guests in one
//! process wait on each other's children, which is not a thing that can be worked around from
//! above. See `docs/ARCHITECTURE.md` §6.2.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use bohr_explorer::{Campaign, RunResult, WorkItem};
use bohr_tape::Tape;
use bohr_types::Seed;

use crate::{execute, guests::Demo, wire, CliError};

/// How many items a generation holds.
///
/// **Fixed, and deliberately not a function of the worker count.** A generation is the unit the
/// campaign plans and folds, so sizing it by `-j` would make the shape of the search a property of
/// the machine it ran on — and then eight workers and one would explore different things from the
/// same master seed, which is the exact promise this design exists to keep. Workers slice a
/// generation; they do not size it, and running with more than this many gives the extra ones
/// nothing to do.
///
/// Thirty-two because a generation ends when its slowest item does: with eight workers that is
/// four items each, so a straggler costs a quarter of a worker's time rather than all of it.
const GENERATION_ITEMS: usize = 32;

/// What to search.
#[derive(Clone, Debug)]
pub(crate) enum Subject {
    /// A guest built into this binary. Needs no privileges, so a campaign runs anywhere.
    Demo(Demo),
    /// A program on disk, under interception.
    Program(Vec<String>),
}

impl Subject {
    /// The arguments that name this subject to a worker.
    fn as_args(&self) -> Vec<String> {
        match self {
            Self::Demo(demo) => vec!["--guest".to_owned(), demo.name().to_owned()],
            Self::Program(argv) => {
                let mut args = vec!["--".to_owned()];
                args.extend(argv.iter().cloned());
                args
            }
        }
    }
}

/// Everything a campaign was asked for.
#[derive(Debug)]
pub(crate) struct Request {
    /// Reproduces the whole campaign.
    pub(crate) master: Seed,
    /// The reproducible budget: this many runs, whatever the machine.
    pub(crate) runs: Option<u64>,
    /// The convenient budget: this long, and the report says how many runs it reached.
    pub(crate) time: Option<Duration>,
    /// How many workers.
    pub(crate) jobs: usize,
    /// Where findings are written.
    pub(crate) findings: PathBuf,
    /// A corpus carried in and out, where one was named.
    pub(crate) corpus: Option<PathBuf>,
    /// Where a machine-readable account of the campaign goes, if one was asked for.
    pub(crate) summary: Option<PathBuf>,
    /// What to search.
    pub(crate) subject: Subject,
    /// Where the scenario was read from, so a worker can read the same file.
    pub(crate) scenario_path: Option<PathBuf>,
    /// The world the search runs in: cluster, network, disk and drawn faults.
    ///
    /// Without one a campaign searches a single node on a quiet network, which is not where the
    /// defects worth hunting live. Carried as the text as well as the parsed form, because every
    /// finding's bundle records the configuration that produced it.
    pub(crate) scenario: Option<(bohr_world::LoadedConfig, String)>,
}

/// Runs a campaign and reports what it found.
///
/// The exit code is the usual one: nothing found is zero, something found is one. A campaign that
/// finds a bug has succeeded, and a pipeline branching on that must not have to parse text.
pub(crate) fn explore(request: &Request) -> Result<u8, CliError> {
    // Named by the process as well as by the seed. Running one master seed twice at once is an
    // ordinary thing to want — the same search at two budgets — and a directory named by the seed
    // alone would have each campaign deleting the other's handover files between generations.
    let scratch = std::env::temp_dir().join(format!(
        "bohr-campaign-{}-{}",
        request.master.get(),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).map_err(|e| {
        CliError::Usage(format!("BOHR-1002: cannot use {}: {e}", scratch.display()))
    })?;
    std::fs::create_dir_all(&request.findings).map_err(|e| {
        CliError::Usage(format!("BOHR-1002: cannot use {}: {e}", request.findings.display()))
    })?;

    let jobs = request.jobs.max(1);
    let mut campaign = Campaign::new(request.master);
    #[allow(clippy::disallowed_methods)]
    let started = Instant::now();
    let mut saved = 0u64;

    // A stored corpus is tapes and nothing else, so what those tapes covered has to be learned by
    // running them. One generation buys back everything a previous campaign found out, and keeps
    // the stored form to a few kilobytes an entry instead of the sixty-four a coverage map costs.
    if let Some(dir) = &request.corpus {
        let carried = load_corpus(dir)?;
        if !carried.is_empty() {
            let results = carry_out(request, &carried, jobs, &scratch)?;
            campaign.adopt(&carried, results);
            eprintln!("carried {} tapes in from {}", carried.len(), dir.display());
        }
    }

    while more_to_do(request, &campaign, started) {
        let size = generation_size(request, &campaign);
        let planned = campaign.plan(size);
        let results = carry_out(request, &planned, jobs, &scratch)?;

        let known: std::collections::BTreeSet<_> = campaign.findings().keys().copied().collect();
        campaign.fold(&planned, results);

        // Saved after folding, so that a finding reached twice in one generation is written once.
        for (digest, finding) in campaign.findings() {
            if known.contains(digest) {
                continue;
            }
            save_finding(request, finding)?;
            saved += 1;
        }
    }

    if let Some(dir) = &request.corpus {
        save_corpus(dir, &campaign)?;
    }
    report(&campaign, started, saved);
    if let Some(path) = &request.summary {
        std::fs::write(path, summary(request, &campaign)).map_err(|e| {
            CliError::Usage(format!("BOHR-1002: cannot write {}: {e}", path.display()))
        })?;
    }
    // A campaign that ended is a campaign whose handover files are of no further use. One that
    // failed keeps them, because the work a worker did not answer for is what someone will look at.
    let _ = std::fs::remove_dir_all(&scratch);

    // **A campaign that armed adversity and landed none has measured a guest under no pressure**,
    // and is indistinguishable from one that pressed hard and was answered well. Refused for the
    // same reason a comparison that compared nothing is: reporting it clean is reporting that a
    // question was asked when it was not. Findings first, because a campaign that found something
    // clearly reached the guest whatever this counted.
    let armed = request.scenario.as_ref().is_some_and(|(c, _)| c.asks_for_adversity());
    if campaign.findings().is_empty() && armed && campaign.adversity() == 0 {
        return Err(CliError::Internal(format!(
            "BOHR-5006: this scenario asks for adversity and none of it landed in {} runs. The \
             runs happened and nothing went wrong in them because nothing was made to; a moment \
             written down or armed after the run ends fires into nothing. Check faults.arm_after \
             ticks against when the run actually ends, which run.ends_with_node decides and no \
             horizon bounds.",
            campaign.runs()
        )));
    }
    Ok(if campaign.findings().is_empty() { crate::exit::CLEAN } else { crate::exit::FINDING })
}

/// The one place in the tool that reads the host clock, and the reason it is allowed to.
///
/// A wall-clock budget cannot be served without one, and §5.5 bans the host clock because a *run*
/// that read it would stop reproducing. This reading cannot reach a run: it decides only whether
/// to plan another generation, it is taken at a generation boundary, and what it decides is
/// recorded as a run count so the same campaign can be repeated exactly with `--runs`. That is
/// also why `--time` is documented as the budget that does not reproduce.
#[allow(clippy::disallowed_methods)]
fn elapsed_since(started: Instant) -> Duration {
    started.elapsed()
}

/// Whether the budget has anything left.
fn more_to_do(request: &Request, campaign: &Campaign, started: Instant) -> bool {
    if let Some(runs) = request.runs {
        return campaign.runs() < runs;
    }
    match request.time {
        // Checked at a generation boundary, never inside one: cutting a generation short would
        // make the campaign depend on when the clock was read, which is the one thing it may not.
        Some(budget) => elapsed_since(started) < budget,
        None => false,
    }
}

/// How many items the next generation holds.
fn generation_size(request: &Request, campaign: &Campaign) -> usize {
    let full = GENERATION_ITEMS;
    match request.runs {
        // Trimmed so a run budget is met exactly. `--runs 10` that ran 12 would be a budget in
        // name only, and two campaigns asked for the same number would have done different work.
        Some(runs) => full.min((runs.saturating_sub(campaign.runs())) as usize).max(1),
        None => full,
    }
}

/// Splits a generation across workers and gathers what they answer.
///
/// Items whose worker died come back missing, and are reissued once. A second failure is reported
/// rather than skipped: a campaign quietly missing a run is a campaign that cannot be repeated,
/// which is worse than one that stopped.
fn carry_out(
    request: &Request,
    planned: &[WorkItem],
    jobs: usize,
    scratch: &Path,
) -> Result<Vec<RunResult>, CliError> {
    let mut gathered = run_slices(request, planned, jobs, scratch, 0)?;

    let answered: std::collections::BTreeSet<u32> = gathered.iter().map(|r| r.index).collect();
    let missing: Vec<WorkItem> =
        planned.iter().filter(|i| !answered.contains(&i.index)).cloned().collect();
    if !missing.is_empty() {
        gathered.extend(run_slices(request, &missing, 1, scratch, 1)?);
    }

    let answered: std::collections::BTreeSet<u32> = gathered.iter().map(|r| r.index).collect();
    if answered.len() != planned.len() {
        return Err(CliError::Internal(format!(
            "BOHR-3006: {} of {} runs never came back, twice; the campaign cannot be repeated \
             from here. The worker's own error is above and says what stopped it — a worker that \
             will not start twice has usually been refused something by the host rather than \
             defeated by the search.",
            planned.len() - answered.len(),
            planned.len()
        )));
    }
    Ok(gathered)
}

/// Spawns one worker per slice and reads back what each wrote.
fn run_slices(
    request: &Request,
    planned: &[WorkItem],
    jobs: usize,
    scratch: &Path,
    attempt: u32,
) -> Result<Vec<RunResult>, CliError> {
    let per = planned.len().div_ceil(jobs.max(1));
    let mut children = Vec::new();

    for (slice, items) in planned.chunks(per.max(1)).enumerate() {
        let work = scratch.join(format!("work-{attempt}-{slice}.bin"));
        let results = scratch.join(format!("results-{attempt}-{slice}.bin"));
        std::fs::write(&work, wire::encode_work(items))
            .map_err(|e| CliError::Internal(format!("BOHR-3006: cannot hand over work: {e}")))?;
        // Removed rather than left to be overwritten. These names are reused by every generation,
        // and a worker that dies before writing would otherwise leave the previous generation's
        // file to be read as this one's answer: the indices are the same 0..n, so results from
        // tapes nobody ran would be folded into the corpus without a word.
        if let Err(e) = std::fs::remove_file(&results) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(CliError::Internal(format!(
                    "BOHR-3006: cannot clear {}: {e}",
                    results.display()
                )));
            }
        }

        let mut command =
            Command::new(std::env::current_exe().map_err(|e| {
                CliError::Internal(format!("BOHR-3006: cannot find this binary: {e}"))
            })?);
        command.arg("explore-worker").arg("--work").arg(&work).arg("--results").arg(&results);
        // Told which world to run in, rather than handed the parsed form: parsing it there is what
        // keeps the two sides reading one file rather than two copies of an idea of it.
        //
        // Before the subject, always. Everything after `--` belongs to the guest, so a flag placed
        // after it is not a flag at all — it is an argument the guest ignores, and the worker then
        // runs the wrong world without either side saying so.
        if let Some(path) = &request.scenario_path {
            command.arg("--config").arg(path);
        }
        command.args(request.subject.as_args());

        let child = command
            .spawn()
            .map_err(|e| CliError::Internal(format!("BOHR-3006: cannot start a worker: {e}")))?;
        children.push((child, results));
    }

    let mut gathered = Vec::new();
    for (mut child, results) in children {
        // A worker that died is not an error here: its items come back missing and are reissued.
        // How it died is said out loud, though — a campaign that reports only "the runs did not
        // come back" sends whoever reads it looking for a defect in the search, when the answer is
        // usually that the machine could not start another process.
        match child.wait() {
            Ok(status) if !status.success() => {
                eprintln!("  a worker exited {status}; its runs will be reissued");
            }
            Err(e) => eprintln!("  a worker could not be waited for: {e}"),
            Ok(_) => {}
        }
        // A worker killed between two runs leaves a file that ends on a record boundary, and every
        // result in it counts. One killed inside a write leaves a record cut in half: that file is
        // reissued whole rather than half-read, because a partial result folded in would enter the
        // corpus as a run that covered less than it did.
        if let Ok(bytes) = std::fs::read(&results) {
            match wire::decode_results(&bytes) {
                Ok(results) => gathered.extend(results),
                Err(e) => eprintln!("  {e}; those runs will be reissued"),
            }
        }
    }
    Ok(gathered)
}

/// Carries out the work in a file. The other half of a worker's life.
///
/// Each result is written as it is earned rather than the slice at the end. A worker is killed by
/// the same things its guests are, and the runs it did complete before that are exactly the ones
/// the campaign would otherwise have to carry out again.
pub(crate) fn work(work_file: &str, results_file: &str, request: &Request) -> Result<u8, CliError> {
    use std::io::Write as _;

    let bytes = std::fs::read(work_file)
        .map_err(|e| CliError::Internal(format!("BOHR-3006: cannot read the work: {e}")))?;
    let items = wire::decode_work(&bytes)?;

    let cannot_write =
        |e: std::io::Error| CliError::Internal(format!("BOHR-3006: cannot write the results: {e}"));
    let mut out = std::fs::File::create(results_file).map_err(cannot_write)?;
    out.write_all(&wire::results_header()).map_err(cannot_write)?;
    for item in &items {
        let result = carry_out_one(request, item)?;
        out.write_all(&wire::encode_result(&result)).map_err(cannot_write)?;
    }
    Ok(crate::exit::CLEAN)
}

/// One run, reduced to what the search is entitled to know about it.
fn carry_out_one(request: &Request, item: &WorkItem) -> Result<RunResult, CliError> {
    let tape = Tape::with_prefix(item.seed, item.prefix.clone());
    let target = target_for(request)?;
    let (scenario, config_text) = match &request.scenario {
        Some((loaded, text)) => (Some(loaded), text.as_str()),
        None => (None, ""),
    };
    // The log is not kept: a campaign runs millions of these, and a trace is a pure function of
    // the tape that produced it. Whoever wants to look at one replays it.
    let completed = execute::run_with(execute::Execution {
        target: &target,
        tape: &tape,
        config: crate::run_config_for(scenario, None),
        output: execute::Output::Discard,
        policy: SCHEDULER,
        record: execute::Record::DigestOnly,
        scenario,
        config_toml: config_text,
    })?;

    Ok(RunResult {
        index: item.index,
        observed: campaign_properties(&completed.sdk_records),
        digest: completed.report.digest,
        outcome: match &completed.report.outcome {
            bohr_trace::Outcome::Clean => None,
            other => Some(format!("{other:?}")),
        },
        coverage: completed.coverage.unwrap_or_default(),
        guest_edges: completed.coverage_edges,
        adversity: completed.report.adversity,
        journal: completed.report.journal,
    })
}

/// How a campaign's runs are interleaved.
///
/// Priority-based, because the search is looking for orderings and a policy with a guarantee about
/// which ones it reaches is worth more here than anywhere else.
const SCHEDULER: &str = "pct";

/// The target to run, from what the campaign was pointed at.
fn target_for(request: &Request) -> Result<execute::Target, CliError> {
    match &request.subject {
        Subject::Demo(demo) => Ok(execute::Target::Demo(demo.name().to_owned(), demo.program())),
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Subject::Program(argv) => {
            // **The whole command line, as `run` and `survey` pass it.** Taking only the program
            // meant `bohr explore -- ./guest --config x.toml` searched `./guest` with no arguments
            // -- a different program, silently: the findings it saved carried the same truncated
            // target, so nothing ever disagreed and said so.
            let loaded = request.scenario.as_ref().map(|(loaded, _)| loaded);
            Ok(execute::Target::Program(crate::specs_for(argv.clone(), loaded, None)))
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        Subject::Program(_) => Err(CliError::Usage(
            "BOHR-1001: this build has no interceptor; search a built-in guest with --guest"
                .to_owned(),
        )),
    }
}

/// Writes the bundle a finding is carried in.
fn save_finding(request: &Request, finding: &bohr_explorer::Finding) -> Result<(), CliError> {
    // Re-run with the log kept. The campaign never held one — a trace is a pure function of the
    // tape — so the bundle is made by running the finding again, which by construction produces
    // the same run.
    let tape = Tape::with_prefix(finding.seed, finding.prefix.clone());
    let target = target_for(request)?;
    let (scenario, config_text) = match &request.scenario {
        Some((loaded, text)) => (Some(loaded), text.as_str()),
        None => (None, ""),
    };
    let completed = execute::once(execute::Execution {
        target: &target,
        tape: &tape,
        config: crate::run_config_for(scenario, None),
        output: execute::Output::Discard,
        policy: SCHEDULER,
        record: execute::Record::Log,
        scenario,
        config_toml: config_text,
    })?;

    let short = format!("{}", finding.digest);
    let short = short.rsplit(':').next().unwrap_or(&short);
    let path = request.findings.join(format!("finding-{}.bohr", &short[..12.min(short.len())]));
    let bytes = execute::to_bundle(&completed, &tape, None)?;
    std::fs::write(&path, bytes)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {}: {e}", path.display())))?;
    eprintln!("  found {} · {}", finding.kind, path.display());
    Ok(())
}

/// What the campaign amounted to.
fn report(campaign: &Campaign, started: Instant, saved: u64) {
    let millis = elapsed_since(started).as_millis().max(1);
    // Integer arithmetic, because floating point is banned in this repository for a reason that
    // applies even here: a number that is nearly the same on two machines is the hardest kind of
    // difference to notice. Carried in hundredths because a traced cluster takes longer than a
    // second a run, and a whole-number rate reports every such campaign as zero. Reported and
    // never decided on — no branch reads it.
    let hundredths = u128::from(campaign.runs()) * 100_000 / millis;

    eprintln!(
        "campaign  {} runs · {} generations · {}.{:02} runs/s · corpus {} · covered {} edges",
        campaign.runs(),
        campaign.generation(),
        hundredths / 100,
        hundredths % 100,
        campaign.corpus().len(),
        campaign.corpus().seen().edges(),
    );
    // Said whether or not anything was found, and for the same reason the map of the untested is:
    // a campaign that injected nothing looks exactly like one that injected plenty and was answered
    // well, and nothing else here separates them. Faults are drawn, so this is a fact about the
    // runs rather than about the configuration that armed them.
    eprintln!("  findings {saved} · adversity landed {}", campaign.adversity());
    // **The number that says whether the feedback still discriminates.** A map with fewer counters
    // than the guest has edges puts two edges on one counter, and two runs that differ only there
    // are one run to the corpus. Said whenever the guest reported an edge count, because a search
    // whose feedback has quietly stopped choosing looks exactly like one that has covered
    // everything there is — and the remedy is a number in the scenario, not more campaigning.
    let guest_edges = campaign.guest_edges();
    if guest_edges > 0 {
        let cells = campaign.corpus().seen().cells() as u64;
        eprintln!("  guest has {guest_edges} edges · map holds {cells} counters");
        if guest_edges > cells {
            eprintln!(
                "  the map is smaller than the guest: raise [coverage] shm_size to at least \
                 {guest_edges}, or the search cannot tell every path apart"
            );
        }
    }
    for (operator, kept) in campaign.credit() {
        eprintln!("  {:<12} {kept} entries", operator.name());
    }

    // The map of the untested, and the reason a campaign that found nothing is still worth
    // reading. A property the guest states and the search never once brought about is a gap in the
    // testing rather than in the guest, and it appears in no crash report — so it is printed here,
    // whether or not anything was found.
    let properties = campaign.properties();
    if !properties.is_empty() {
        let unsatisfied = properties.unsatisfied();
        eprintln!(
            "  properties  {} stated · {} never satisfied",
            properties.tallies().len(),
            unsatisfied.len()
        );
        for name in unsatisfied {
            eprintln!("    never  {name}");
        }
    }
}

/// The campaign as one machine-readable object.
///
/// **Written through a serialiser, because the strings in it are the guest's.** It used to be
/// assembled by hand: a finding's kind is `format!("{:?}")` of an outcome, so
/// `PropertyViolated("log is monotonic")` put bare double quotes inside a JSON string, and a
/// property name comes from a record the guest wrote — so a guest naming a property
/// `x", "satisfied": 999999, "y` rewrote the machine-readable account of the campaign that ran it.
/// The gate compares these byte for byte and the test only asked whether one substring appeared,
/// so an invalid file shipped as soon as anything was found.
fn summary(request: &Request, campaign: &Campaign) -> String {
    let findings: Vec<serde_json::Value> = campaign
        .findings()
        .values()
        .map(|f| {
            serde_json::json!({
                "digest": f.digest.to_string(),
                "kind": f.kind,
                "generation": f.generation,
            })
        })
        .collect();

    let credit: serde_json::Map<String, serde_json::Value> = campaign
        .credit()
        .iter()
        .map(|(op, kept)| (op.name().to_owned(), serde_json::json!(kept)))
        .collect();

    // Every property the guests stated, satisfied or not. The unsatisfied ones are the map of
    // what was never tested, and they are the reason this section exists at all.
    let properties: Vec<serde_json::Value> = campaign
        .properties()
        .tallies()
        .iter()
        .map(|(name, tally)| {
            serde_json::json!({
                "name": name,
                "evaluations": tally.evaluations,
                "satisfied": tally.satisfied,
                "first_satisfied": tally.first_satisfied.map(|run| run.to_string()),
            })
        })
        .collect();

    let account = serde_json::json!({
        "master_seed": request.master.get(),
        "runs": campaign.runs(),
        "generations": campaign.generation(),
        "corpus": campaign.corpus().len(),
        "evicted": campaign.corpus().evicted(),
        "adversity": campaign.adversity(),
        "guest_edges": campaign.guest_edges(),
        "map_cells": campaign.corpus().seen().cells(),
        "covered": campaign.corpus().seen().digest().to_string(),
        // Names the campaign had no room for, so a map of the untested that stopped growing says
        // so rather than reading as a guest with few promises.
        "properties_refused": campaign.properties().refused(),
        "findings": findings,
        "kept_by_operator": credit,
        "properties": properties,
    });

    // Pretty, because a person reads this as often as a machine does, and stable because the gate
    // compares two of them byte for byte.
    let mut text = serde_json::to_string_pretty(&account).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

/// Reads the tapes a previous campaign left behind.
///
/// Damage is skipped rather than refused: a corpus is somebody's working directory, and a campaign
/// that will not start because one file in four thousand is truncated has confused an
/// inconvenience with a defect. What it may not do is read a damaged file as a different tape,
/// which is why the handover format checks itself.
fn load_corpus(dir: &Path) -> Result<Vec<WorkItem>, CliError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tape"))
        .collect();
    // Sorted, because the order entries are re-run in decides the order they enter the corpus,
    // and a directory listing is not a stable order on any filesystem.
    paths.sort();

    let mut carried = Vec::new();
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if let Ok(items) = wire::decode_work(&bytes) {
            for item in items {
                carried.push(WorkItem { index: carried.len() as u32, ..item });
            }
        }
    }
    Ok(carried)
}

/// Writes the corpus out, one tape per file.
fn save_corpus(dir: &Path, campaign: &Campaign) -> Result<(), CliError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot use {}: {e}", dir.display())))?;
    // Cleared first: an entry the campaign evicted must not survive in the directory and be
    // carried back in next time, which would make a corpus that only ever grows.
    if let Ok(existing) = std::fs::read_dir(dir) {
        for path in existing.flatten().map(|e| e.path()) {
            if path.extension().is_some_and(|x| x == "tape") {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    for (n, entry) in campaign.corpus().entries().iter().enumerate() {
        let item = WorkItem {
            index: n as u32,
            seed: entry.seed,
            prefix: entry.prefix.clone(),
            origin: entry.origin,
        };
        let path = dir.join(format!("entry-{n:05}.tape"));
        std::fs::write(&path, wire::encode_work(std::slice::from_ref(&item))).map_err(|e| {
            CliError::Usage(format!("BOHR-1002: cannot write {}: {e}", path.display()))
        })?;
    }
    Ok(())
}

/// The campaign-scoped promises a guest made in this run.
///
/// Only `sometimes` and `reachable`: whether an `always` held is this run's own verdict and is
/// already an outcome. Whether a `sometimes` has *ever* held is a question about the campaign, and
/// these are what a campaign answers it from.
///
/// A line that does not parse is skipped. The supervisor already counts and reports malformed
/// records when it takes them; counting them twice here would say the guest wrote twice as much
/// nonsense as it did.
fn campaign_properties(records: &[u8]) -> Vec<(String, bool)> {
    String::from_utf8_lossy(records)
        .lines()
        .filter_map(|line| match bohr_sdk_proto::Record::parse_line(line) {
            Ok(bohr_sdk_proto::Record::Assert(assertion, _))
                if !assertion.assert_type.is_per_run() =>
            {
                Some((assertion.message, assertion.condition))
            }
            _ => None,
        })
        .collect()
}
