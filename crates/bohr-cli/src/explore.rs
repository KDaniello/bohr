//! Driving a campaign, and the workers that carry it out.
//!
//! The search lives in `bohr-explorer`, which has no processes. This is the plumbing: hand a
//! generation's items to free workers, gather the results, save what was found. Reproducibility is
//! decided in the explorer; this file can only be slow or lose a result, and says so if it does.
//!
//! Workers are processes. A guest that confuses the kernel's `ptrace` bookkeeping takes down one
//! worker only, and two traced guests in one process would wait on each other's
//! children. See `docs/ARCHITECTURE.md` §6.2.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use bohr_explorer::{Campaign, RunResult, WorkItem};
use bohr_tape::Tape;
use bohr_types::Seed;

use crate::{CliError, execute, guests::Demo, wire};

/// How many items a generation holds.
///
/// Fixed, independent of `-j`, so one master seed explores the same things on any number of
/// workers. Workers beyond this many have nothing to do.
///
/// A generation ends with its slowest item; 32 gives eight workers four items each, so a straggler
/// costs a quarter of a worker's time.
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
    /// Where the campaign is written down between generations, and resumed from if it already is.
    pub(crate) resume: Option<PathBuf>,
    /// Where a machine-readable account of the campaign goes, if one was asked for.
    pub(crate) summary: Option<PathBuf>,
    /// What to search.
    pub(crate) subject: Subject,
    /// Where the scenario was read from, so a worker can read the same file.
    pub(crate) scenario_path: Option<PathBuf>,
    /// The world the search runs in: cluster, network, disk and drawn faults.
    ///
    /// Without one the campaign searches a single node on a quiet network. Kept as text too, since
    /// every finding's bundle records it.
    pub(crate) scenario: Option<(bohr_world::LoadedConfig, String)>,
}

/// Runs a campaign and reports what it found.
///
/// Exits 0 when nothing was found and 1 when something was.
pub(crate) fn explore(request: &Request) -> Result<u8, CliError> {
    std::fs::create_dir_all(&request.findings).map_err(|e| {
        CliError::Usage(format!("BOHR-1002: cannot use {}: {e}", request.findings.display()))
    })?;

    let jobs = request.jobs.max(1);
    let mut pool = Pool::start(request, jobs)?;
    let (mut campaign, mut pending) = match &request.resume {
        Some(dir) => resume::load(dir, request)?,
        None => (Campaign::new(request.master), Vec::new()),
    };
    #[allow(clippy::disallowed_methods)]
    let started = Instant::now();
    let mut saved: Vec<PathBuf> = Vec::new();

    // A stored corpus holds only tapes, so their coverage is learned by running them: one
    // generation, and a few KB per entry instead of a 64 KB map. A resumed campaign already has
    // its corpus.
    if let Some(dir) = &request.corpus
        && campaign.runs() == 0
        && pending.is_empty()
    {
        let carried = load_corpus(dir)?;
        if !carried.is_empty() {
            let results = carry_out(&mut pool, &carried)?;
            campaign.adopt(&carried, results);
            eprintln!("carried {} tapes in from {}", carried.len(), dir.display());
        }
    }

    // A generation is folded only when whole. One cut short by a run budget is held as results; a
    // resumed campaign runs the rest and folds it all, as an uninterrupted one would. The report
    // folds the held part into a copy.
    while more_to_do(request, campaign.runs() + pending.len() as u64, started) {
        let planned = campaign.plan(GENERATION_ITEMS);
        let size = generation_size(request, campaign.runs()).max(pending.len());
        let results = carry_out(&mut pool, &planned[pending.len()..size])?;
        pending.extend(results);
        if pending.len() < GENERATION_ITEMS {
            continue;
        }
        let known: std::collections::BTreeSet<_> = campaign.findings().keys().copied().collect();
        campaign.fold(&planned, std::mem::take(&mut pending));
        // Saved after folding, so a finding reached twice in a generation is written once.
        saved.extend(save_new_findings(request, &campaign, &known)?);
        // Saved after every whole generation, so a stopped campaign resumes from the last one.
        if let Some(dir) = &request.resume {
            resume::save(dir, request, &campaign, &pending)?;
        }
    }

    if let Some(dir) = &request.resume {
        resume::save(dir, request, &campaign, &pending)?;
    }
    drop(pool);
    let mut view = campaign.clone();
    if !pending.is_empty() {
        let planned = campaign.plan(GENERATION_ITEMS);
        let known: std::collections::BTreeSet<_> = campaign.findings().keys().copied().collect();
        view.fold(&planned[..pending.len()], pending);
        saved.extend(save_new_findings(request, &view, &known)?);
    }
    let campaign = view;

    if let Some(dir) = &request.corpus {
        save_corpus(dir, &campaign)?;
    }
    report(&campaign, started, saved.len() as u64);
    if let Some(path) = &request.summary {
        std::fs::write(path, summary(request, &campaign)).map_err(|e| {
            CliError::Usage(format!("BOHR-1002: cannot write {}: {e}", path.display()))
        })?;
    }

    // A campaign that armed faults and landed none tested nothing, so it is refused instead of
    // reported clean. Findings are checked first: a campaign that found something reached the
    // guest.
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
    if crate::json::on() {
        let elapsed_ms = u64::try_from(elapsed_since(started).as_millis()).unwrap_or(u64::MAX);
        crate::json::emit("explore", result(request, &campaign, &saved, elapsed_ms));
    }
    Ok(if campaign.findings().is_empty() { crate::exit::CLEAN } else { crate::exit::FINDING })
}

/// Reads the host clock for a wall-clock budget.
///
/// §5.5 bans the clock inside runs. This is read only at generation boundaries to decide whether
/// to plan another; the result is recorded as a run count, so `--runs` repeats the campaign
/// exactly. `--time` itself does not reproduce.
#[allow(clippy::disallowed_methods)]
fn elapsed_since(started: Instant) -> Duration {
    started.elapsed()
}

/// Writes the findings a fold added, and says where.
fn save_new_findings(
    request: &Request,
    campaign: &Campaign,
    known: &std::collections::BTreeSet<bohr_types::Digest>,
) -> Result<Vec<PathBuf>, CliError> {
    let mut saved = Vec::new();
    for (digest, finding) in campaign.findings() {
        if !known.contains(digest) {
            saved.extend(save_finding(request, finding)?);
        }
    }
    Ok(saved)
}

/// Whether the budget has anything left, given the runs done so far.
fn more_to_do(request: &Request, done: u64, started: Instant) -> bool {
    if let Some(runs) = request.runs {
        return done < runs;
    }
    match request.time {
        // Checked only between generations, so the clock never cuts one short.
        Some(budget) => elapsed_since(started) < budget,
        None => false,
    }
}

/// How many items of the next generation to run, given the runs already folded.
fn generation_size(request: &Request, folded: u64) -> usize {
    let full = GENERATION_ITEMS;
    match request.runs {
        // Trimmed so `--runs` is met exactly.
        Some(runs) => full.min((runs.saturating_sub(folded)) as usize).max(1),
        None => full,
    }
}

/// Carries out `planned` on the pool and gathers what the workers answer.
///
/// An item whose worker died is reissued once. A second failure is an error, since a campaign
/// missing a run could not be repeated.
fn carry_out(pool: &mut Pool, planned: &[WorkItem]) -> Result<Vec<RunResult>, CliError> {
    let mut queue: std::collections::VecDeque<WorkItem> = planned.iter().cloned().collect();
    let mut failed: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut gathered = Vec::with_capacity(planned.len());
    let mut in_flight = 0usize;
    loop {
        // Every idle worker is handed the next item, so no worker waits while there is work.
        while let Some(slot) = pool.idle() {
            let Some(item) = queue.pop_front() else { break };
            pool.hand(slot, item)?;
            in_flight += 1;
        }
        if in_flight == 0 {
            break;
        }
        let (slot, answer) = pool.next_answer()?;
        in_flight -= 1;
        match answer {
            Ok(results) => gathered.extend(results),
            Err((item, why)) => {
                eprintln!("  a worker stopped: {why}; its run will be reissued");
                if !failed.insert(item.index) {
                    return Err(CliError::Internal(format!(
                        "BOHR-3006: run {} never came back, twice; the campaign cannot be \
                         repeated from here. The worker's own error is above and says what \
                         stopped it — a worker that will not start twice has usually been refused \
                         something by the host rather than defeated by the search.",
                        item.index
                    )));
                }
                pool.restart(slot)?;
                queue.push_front(item);
            }
        }
    }
    Ok(gathered)
}

/// What one worker answered for one item: its results, or the item back with why it was not run.
type Answer = Result<Vec<RunResult>, (WorkItem, String)>;

/// Workers that live for the whole campaign, each handed the next item as it finishes the last.
///
/// A generation ends when its last item does, so an item goes to whichever worker is free. Items go
/// in on a worker's stdin and results come back on its stdout, read by a thread per worker.
struct Pool {
    request_args: Vec<String>,
    workers: Vec<Worker>,
    answers: std::sync::mpsc::Receiver<(usize, u64, Option<Vec<u8>>)>,
    sender: std::sync::mpsc::Sender<(usize, u64, Option<Vec<u8>>)>,
}

/// One worker process and the item it is carrying out.
struct Worker {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    busy: Option<WorkItem>,
    /// Which incarnation of this slot the process is, to ignore messages from a replaced one.
    life: u64,
}

impl Pool {
    fn start(request: &Request, jobs: usize) -> Result<Self, CliError> {
        let mut args = vec!["explore-worker".to_owned(), "--serve".to_owned()];
        // Before the subject: everything after `--` goes to the guest.
        if let Some(path) = &request.scenario_path {
            args.push("--config".to_owned());
            args.push(path.to_string_lossy().into_owned());
        }
        args.extend(request.subject.as_args());
        let (sender, answers) = std::sync::mpsc::channel();
        let mut pool = Self { request_args: args, workers: Vec::new(), answers, sender };
        for slot in 0..jobs.max(1) {
            let worker = pool.spawn(slot, 0)?;
            pool.workers.push(worker);
        }
        Ok(pool)
    }

    fn spawn(&self, slot: usize, life: u64) -> Result<Worker, CliError> {
        let mut command =
            Command::new(std::env::current_exe().map_err(|e| {
                CliError::Internal(format!("BOHR-3006: cannot find this binary: {e}"))
            })?);
        command.args(&self.request_args);
        command.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        // One core per worker, shared by its supervisor and guests: a stop across cores is a
        // cross-core wake-up. The backend maps the position to an allowed core.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        command.env(bohr_backend_ptrace::CORE_VARIABLE, slot.to_string());
        let mut child = command
            .spawn()
            .map_err(|e| CliError::Internal(format!("BOHR-3006: cannot start a worker: {e}")))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let Some(mut stdout) = stdout else {
                let _ = sender.send((slot, life, None));
                return;
            };
            loop {
                let frame = read_frame(&mut stdout);
                let stop = frame.is_none();
                if sender.send((slot, life, frame)).is_err() || stop {
                    return;
                }
            }
        });
        Ok(Worker { child, stdin, busy: None, life })
    }

    /// A worker with nothing to do, if there is one.
    fn idle(&self) -> Option<usize> {
        self.workers.iter().position(|w| w.busy.is_none())
    }

    /// Hands one item to a worker, starting it again first where it has gone.
    ///
    /// A worker that died idle is replaced without charging the item; only dying with the item
    /// counts against it.
    fn hand(&mut self, slot: usize, item: WorkItem) -> Result<(), CliError> {
        let frame = framed(&wire::encode_work(std::slice::from_ref(&item)));
        for _ in 0..2 {
            let written = self.workers[slot]
                .stdin
                .as_mut()
                .is_some_and(|stdin| std::io::Write::write_all(stdin, &frame).is_ok());
            if written {
                self.workers[slot].busy = Some(item);
                return Ok(());
            }
            self.restart(slot)?;
        }
        Err(CliError::Internal(format!(
            "BOHR-3006: worker {slot} would not take work twice in a row; the host is refusing \
             to start it"
        )))
    }

    /// The next answer any worker gives, with the slot it came from.
    fn next_answer(&mut self) -> Result<(usize, Answer), CliError> {
        loop {
            let (slot, life, frame) = self.answers.recv().map_err(|_| {
                CliError::Internal("BOHR-3006: every worker's reader has gone".to_owned())
            })?;
            let Some(worker) = self.workers.get_mut(slot) else { continue };
            // Ignore messages from a replaced process.
            if life != worker.life {
                continue;
            }
            let Some(item) = worker.busy.take() else {
                // Unsolicited output, or an idle worker ending; it restarts when next given work.
                if frame.is_none() {
                    worker.stdin = None;
                }
                continue;
            };
            let answer = match frame {
                Some(bytes) => match wire::decode_results(&bytes) {
                    Ok(results) if results.iter().any(|r| r.index == item.index) => Ok(results),
                    Ok(_) => Err((item, "it answered for another run".to_owned())),
                    Err(e) => Err((item, e.to_string())),
                },
                None => {
                    let status =
                        worker.child.wait().map_or_else(|e| e.to_string(), |s| s.to_string());
                    worker.stdin = None;
                    Err((item, format!("it exited {status}")))
                }
            };
            return Ok((slot, answer));
        }
    }

    /// Replaces a worker that failed, in the same slot and so on the same core.
    fn restart(&mut self, slot: usize) -> Result<(), CliError> {
        let life = self.workers[slot].life + 1;
        let old = &mut self.workers[slot];
        old.stdin = None;
        let _ = old.child.kill();
        let _ = old.child.wait();
        self.workers[slot] = self.spawn(slot, life)?;
        Ok(())
    }
}

impl Drop for Pool {
    /// Idle workers see their stdin close; busy ones are killed, since the campaign has failed.
    fn drop(&mut self) {
        for worker in &mut self.workers {
            worker.stdin = None;
            if worker.busy.is_some() {
                let _ = worker.child.kill();
            }
        }
        for worker in &mut self.workers {
            let _ = worker.child.wait();
        }
    }
}

/// A frame: its length, then its bytes.
fn framed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 8);
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Reads one frame, or nothing at the end of the stream or on a broken one.
fn read_frame(from: &mut impl std::io::Read) -> Option<Vec<u8>> {
    let mut length = [0u8; 8];
    from.read_exact(&mut length).ok()?;
    let length = usize::try_from(u64::from_le_bytes(length)).ok()?;
    // A result is a coverage map and a journal; a frame larger than this is a broken stream.
    if length > 1 << 30 {
        return None;
    }
    let mut bytes = vec![0u8; length];
    from.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// Serves items from stdin until it closes: the other half of a pooled worker's life.
///
/// Each result is flushed before the next item is read, so a worker killed between runs has
/// reported every run it finished.
pub(crate) fn serve(request: &Request) -> Result<u8, CliError> {
    use std::io::Write as _;

    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let cannot_write =
        |e: std::io::Error| CliError::Internal(format!("BOHR-3006: cannot write a result: {e}"));
    while let Some(frame) = read_frame(&mut input) {
        let mut answer = wire::results_header();
        for item in wire::decode_work(&frame)? {
            answer.extend_from_slice(&wire::encode_result(&carry_out_one(request, &item)?));
        }
        output.write_all(&framed(&answer)).map_err(cannot_write)?;
        output.flush().map_err(cannot_write)?;
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
    // No log: a trace follows from its tape, so anyone who needs it replays the run.
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
/// Priority-based (PCT), which guarantees which orderings it reaches.
const SCHEDULER: &str = "pct";

/// The target to run, from what the campaign was pointed at.
fn target_for(request: &Request) -> Result<execute::Target, CliError> {
    match &request.subject {
        Subject::Demo(demo) => Ok(execute::Target::Demo(demo.name().to_owned(), demo.program())),
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Subject::Program(argv) => {
            // The whole command line, as `run` and `survey` pass it.
            let loaded = request.scenario.as_ref().map(|(loaded, _)| loaded);
            Ok(execute::Target::Program(crate::specs_for(argv.clone(), loaded, None)?))
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        Subject::Program(_) => Err(CliError::Usage(
            "BOHR-1001: this build has no interceptor; search a built-in guest with --guest"
                .to_owned(),
        )),
    }
}

/// Writes a finding's bundle and returns its path. A bundle already under its digest's name is the
/// same run, written before the campaign was resumed.
fn save_finding(
    request: &Request,
    finding: &bohr_explorer::Finding,
) -> Result<Option<PathBuf>, CliError> {
    let short = format!("{}", finding.digest);
    let short = short.rsplit(':').next().unwrap_or(&short);
    let path = request.findings.join(format!("finding-{}.bohr", &short[..12.min(short.len())]));
    if path.is_file() {
        return Ok(None);
    }
    // Re-run with the log kept; the same tape gives the same run.
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

    let bytes = execute::to_bundle(&completed, &tape, None)?;
    std::fs::write(&path, bytes)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {}: {e}", path.display())))?;
    let verdict = bohr_report::narrate::verdict(&completed.report.outcome);
    eprintln!("  found  {}    {} · {}", verdict.headline, verdict.code, path.display());
    Ok(Some(path))
}

/// What the campaign amounted to.
fn report(campaign: &Campaign, started: Instant, saved: u64) {
    let millis = elapsed_since(started).as_millis().max(1);
    // Integer arithmetic; the repository bans floating point. Hundredths, because a traced cluster
    // takes over a second a run and a whole-number rate would read zero. Only reported.
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
    // Always printed, counting faults that landed, so a campaign that injected nothing shows.
    eprintln!("  findings {saved} · adversity landed {}", campaign.adversity());
    if saved > 0 {
        eprintln!("  next     bohr findings <dir> groups them; bohr explain <bundle> tells one");
    }
    // Whether the map still tells edges apart: with fewer counters than edges, runs that differ
    // only there look the same. Printed whenever the guest reports an edge count; the remedy is
    // the map size in the scenario.
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

    // Properties the guest stated and the search never satisfied: gaps in the testing. Printed
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
/// Built with a serialiser, because property names come from the guest and hand-written JSON
/// would let a guest inject fields.
fn account(request: &Request, campaign: &Campaign) -> serde_json::Value {
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

    // Every property the guests stated; the unsatisfied ones show what was never tested.
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

    serde_json::json!({
        "master_seed": request.master.get(),
        "runs": campaign.runs(),
        "generations": campaign.generation(),
        "corpus": campaign.corpus().len(),
        "evicted": campaign.corpus().evicted(),
        "adversity": campaign.adversity(),
        "guest_edges": campaign.guest_edges(),
        "map_cells": campaign.corpus().seen().cells(),
        "covered": campaign.corpus().seen().digest().to_string(),
        // Names the campaign had no room for, so a capped list is not read as a short one.
        "properties_refused": campaign.properties().refused(),
        "findings": findings,
        "kept_by_operator": credit,
        "properties": properties,
    })
}

/// The `--summary` file: the account, pretty-printed for people and stable for byte comparison.
fn summary(request: &Request, campaign: &Campaign) -> String {
    let mut text = serde_json::to_string_pretty(&account(request, campaign))
        .unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

/// The `--json` object: the account, and what this invocation wrote and how long it took.
///
/// `saved` is the bundles written by this invocation only; a resumed campaign's earlier findings
/// are in `findings` and already on disk. `elapsed_ms` is the host clock and never reproduces.
fn result(
    request: &Request,
    campaign: &Campaign,
    saved: &[PathBuf],
    elapsed_ms: u64,
) -> serde_json::Value {
    let shown = |path: &Path| path.display().to_string();
    let mut value = account(request, campaign);
    if let serde_json::Value::Object(fields) = &mut value {
        fields.extend([
            ("found".to_owned(), serde_json::json!(!campaign.findings().is_empty())),
            ("findings_dir".to_owned(), serde_json::json!(shown(&request.findings))),
            (
                "saved".to_owned(),
                serde_json::json!(saved.iter().map(|p| shown(p)).collect::<Vec<_>>()),
            ),
            ("summary".to_owned(), serde_json::json!(request.summary.as_deref().map(shown))),
            ("corpus_dir".to_owned(), serde_json::json!(request.corpus.as_deref().map(shown))),
            ("resume_dir".to_owned(), serde_json::json!(request.resume.as_deref().map(shown))),
            ("elapsed_ms".to_owned(), serde_json::json!(elapsed_ms)),
        ]);
    }
    value
}

/// Reads the tapes a previous campaign left behind.
///
/// A damaged file is skipped, so one bad entry does not stop a campaign. The format checks itself,
/// so a damaged file is never read as a different tape.
fn load_corpus(dir: &Path) -> Result<Vec<WorkItem>, CliError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tape"))
        .collect();
    // Sorted: the order decides corpus order, and directory listings are not stable.
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
    // Cleared first, so evicted entries do not come back next time.
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
/// Only `sometimes` and `reachable`; an `always` is already this run's outcome.
///
/// Unparsable lines are skipped; the supervisor already counts and reports them.
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

/// A campaign written down in a directory between generations, and taken up again.
///
/// `campaign.bin` holds the explorer's versioned state and any unfolded results, written to a part
/// file and renamed, so a stop leaves either save whole. `campaign.toml` holds what a resume is
/// checked against: the master seed and the question (program digest, arguments, scenario). A
/// directory for another question is refused.
mod resume {
    use std::path::Path;

    use bohr_explorer::{Campaign, RunResult};

    use super::{Request, Subject};
    use crate::{CliError, wire};

    fn refused(text: String) -> CliError {
        CliError::Usage(format!("BOHR-1002: {text}"))
    }

    /// What the campaign was asked, so a resume can tell this question from another.
    fn question(request: &Request) -> String {
        let mut asked = format!("{:?}", request.subject);
        if let Subject::Program(argv) = &request.subject
            && let Some(bytes) = argv.first().and_then(|p| std::fs::read(p).ok())
        {
            asked.push_str(&bohr_types::Digest::of(&bytes).to_hex());
        }
        if let Some((_, text)) = &request.scenario {
            asked.push_str(text);
        }
        bohr_trace::short_digest(asked.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The campaign a directory holds, or a new one where it holds none.
    pub(super) fn load(
        dir: &Path,
        request: &Request,
    ) -> Result<(Campaign, Vec<RunResult>), CliError> {
        let state = dir.join("campaign.bin");
        if !state.is_file() {
            return Ok((Campaign::new(request.master), Vec::new()));
        }
        let header = std::fs::read_to_string(dir.join("campaign.toml"))
            .map_err(|e| refused(format!("cannot read {}: {e}", dir.display())))?;
        let field = |key: &str| {
            header.lines().find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|rest| rest.trim_start().strip_prefix('='))
                    .map(|value| value.trim().trim_matches('"').to_owned())
            })
        };
        if field("master_seed") != Some(request.master.get().to_string()) {
            return Err(refused(format!(
                "{} holds a campaign with master seed {}, not {}",
                dir.display(),
                field("master_seed").unwrap_or_default(),
                request.master.get()
            )));
        }
        if field("question") != Some(question(request)) {
            return Err(refused(format!(
                "{} holds a campaign of another program, binary or scenario; resume it with the \
                 ones it was started with, or name a new directory",
                dir.display()
            )));
        }
        let bytes = std::fs::read(&state)
            .map_err(|e| refused(format!("cannot read {}: {e}", state.display())))?;
        let damaged = || refused(format!("{} is damaged", state.display()));
        let len = bytes
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
            .ok_or_else(damaged)?;
        let split = usize::try_from(len)
            .ok()
            .and_then(|n| n.checked_add(8))
            .filter(|n| *n <= bytes.len())
            .ok_or_else(damaged)?;
        let campaign = Campaign::from_state(&bytes[8..split]).map_err(|e| refused(e.0))?;
        let pending = wire::decode_results(&bytes[split..])?;
        eprintln!(
            "resumed {} at {} runs, generation {}",
            dir.display(),
            campaign.runs() + pending.len() as u64,
            campaign.generation()
        );
        Ok((campaign, pending))
    }

    /// Writes the campaign down whole, replacing the last save in one rename.
    pub(super) fn save(
        dir: &Path,
        request: &Request,
        campaign: &Campaign,
        pending: &[RunResult],
    ) -> Result<(), CliError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| refused(format!("cannot use {}: {e}", dir.display())))?;
        let state = campaign.to_state();
        let mut bytes = (state.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(&state);
        bytes.extend_from_slice(&wire::results_header());
        for result in pending {
            bytes.extend_from_slice(&wire::encode_result(result));
        }
        let header = format!(
            "# A campaign bohr can resume: bohr explore --resume <this directory>.\n\
             master_seed = {}\nruns = {}\ngeneration = {}\nquestion = \"{}\"\n",
            request.master.get(),
            campaign.runs() + pending.len() as u64,
            campaign.generation(),
            question(request)
        );
        // The header first: its fields never change, so it matches either state file.
        for (name, content) in [("campaign.toml", header.into_bytes()), ("campaign.bin", bytes)] {
            let at = dir.join(name);
            let part = dir.join(format!("{name}.part"));
            std::fs::write(&part, content)
                .and_then(|()| std::fs::rename(&part, &at))
                .map_err(|e| refused(format!("cannot write {}: {e}", at.display())))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The object is the `--summary` account plus what this invocation wrote; the two agree.
    #[test]
    fn the_object_is_the_summary_and_what_was_written() {
        let request = Request {
            master: Seed(7),
            runs: Some(0),
            time: None,
            jobs: 1,
            findings: PathBuf::from("out/findings"),
            corpus: None,
            resume: None,
            summary: Some(PathBuf::from("out/summary.json")),
            subject: Subject::Demo(Demo::ManyFlips),
            scenario_path: None,
            scenario: None,
        };
        let campaign = Campaign::new(request.master);
        let saved = [PathBuf::from("out/findings/finding-0123456789ab.bohr")];
        let value = crate::json::object("explore", result(&request, &campaign, &saved, 12));
        assert_eq!(value["schema"], "bohr.explore/1");
        assert_eq!(value["master_seed"], 7);
        assert_eq!(value["runs"], 0);
        assert_eq!(value["found"], false);
        assert_eq!(value["findings"], serde_json::json!([]));
        assert_eq!(value["saved"][0], "out/findings/finding-0123456789ab.bohr");
        assert_eq!(value["summary"], "out/summary.json");
        assert_eq!(value["corpus_dir"], serde_json::Value::Null);
        assert_eq!(value["elapsed_ms"], 12);

        let file: serde_json::Value = serde_json::from_str(&summary(&request, &campaign)).unwrap();
        for (key, field) in file.as_object().unwrap() {
            assert_eq!(&value[key], field, "{key} differs from the summary file");
        }
    }
}
