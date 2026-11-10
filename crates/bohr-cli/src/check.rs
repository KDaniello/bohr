//! `bohr check` and `bohr sweep`: one set of checks, held against any subject.
//!
//! **A check is not a test about one program, but a property of runs.** It takes a scenario and a
//! program, starts `bohr run` on them as many times as it needs, and reads back what each run came
//! to through `--facts`. So `repeats` is the same code for geth and for a forty-line C guest, and a
//! gate is a line in a subject's file rather than a script beside it.
//!
//! Runs are separate processes, as a campaign's and a hunt's are: a run that wedges or dies takes
//! its own process with it and leaves the count it belongs to intact.
//!
//! What this does not do: say what a program means. Checks are about runs — that they repeat, stay
//! clean without faults, find what was seeded — and what a run is judged by is still the scenario's
//! oracle, the guest's SDK assertions and its probes.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use bohr_backend_ptrace::CORE_VARIABLE;

use crate::{
    CliError,
    facts::Facts,
    subjects::{Check, Kind, Metric, Milli, Performs, Subject},
};
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
const CORE_VARIABLE: &str = "BOHR_CORE";

fn refused(text: impl std::fmt::Display) -> CliError {
    CliError::Usage(format!("BOHR-1002: {text}"))
}

fn absolute(path: &Path) -> String {
    std::path::absolute(path).unwrap_or_else(|_| path.to_owned()).to_string_lossy().into_owned()
}

/// What one run left behind.
#[derive(Clone, Debug)]
pub(crate) struct Ran {
    pub(crate) seed: u64,
    /// What the run came to, where it came to anything.
    pub(crate) facts: Option<Facts>,
    /// The last line it wrote to standard error: why it did not complete, where it did not.
    pub(crate) said: String,
    pub(crate) wall: Duration,
}

/// Starts runs of one program, in parallel, each on a core of its own.
pub(crate) struct Runner {
    me: PathBuf,
    cwd: PathBuf,
    scratch: PathBuf,
    jobs: usize,
}

/// Names each facts file once per process, so two runs of one seed never share one.
static NEXT: AtomicU64 = AtomicU64::new(0);

impl Runner {
    pub(crate) fn new(cwd: &Path, scratch: &Path, jobs: usize) -> Result<Self, CliError> {
        std::fs::create_dir_all(scratch)
            .map_err(|e| refused(format!("cannot use {}: {e}", scratch.display())))?;
        Ok(Self {
            me: std::env::current_exe().map_err(|e| refused(format!("cannot find bohr: {e}")))?,
            cwd: cwd.to_owned(),
            scratch: scratch.to_owned(),
            jobs: jobs.max(1),
        })
    }

    /// One run of a seed, with `extra` arguments before the program.
    fn one(&self, scenario: &Path, program: &str, seed: u64, slot: usize, extra: &[String]) -> Ran {
        let file =
            self.scratch.join(format!("facts-{}.json", NEXT.fetch_add(1, Ordering::Relaxed)));
        let started = now();
        let output = Command::new(&self.me)
            .args(["run", "--seed", &seed.to_string(), "--config", &absolute(scenario), "--facts"])
            .arg(&file)
            .args(extra)
            .arg("--")
            .arg(program)
            .current_dir(&self.cwd)
            .env(CORE_VARIABLE, slot.to_string())
            .output();
        let wall = started.elapsed();
        let facts = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<Facts>(&text).ok());
        let _ = std::fs::remove_file(&file);
        let said = output.as_ref().map_or_else(
            |e| e.to_string(),
            |o| String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("").trim().to_owned(),
        );
        Ran { seed, facts, said, wall }
    }

    /// Runs every seed once, `jobs` at a time; the answers come back in the order of `seeds`.
    pub(crate) fn many(&self, scenario: &Path, program: &str, seeds: &[u64]) -> Vec<Ran> {
        let next = AtomicUsize::new(0);
        let done = std::sync::Mutex::new(Vec::with_capacity(seeds.len()));
        std::thread::scope(|scope| {
            for slot in 0..self.jobs.min(seeds.len().max(1)) {
                let (next, done) = (&next, &done);
                scope.spawn(move || {
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(seed) = seeds.get(at) else { break };
                        let ran = self.one(scenario, program, *seed, slot, &[]);
                        if let Ok(mut done) = done.lock() {
                            done.push((at, ran));
                        }
                    }
                });
            }
        });
        let mut done = done.into_inner().unwrap_or_default();
        done.sort_by_key(|(at, _)| *at);
        done.into_iter().map(|(_, ran)| ran).collect()
    }

    /// The scenario with every fault taken out, written beside the runs.
    fn without_faults(&self, scenario: &Path) -> Result<PathBuf, CliError> {
        let text = std::fs::read_to_string(scenario)
            .map_err(|e| refused(format!("cannot read {}: {e}", scenario.display())))?;
        let calm = crate::hunt::without_faults(&text)?;
        let file = self.scratch.join(format!("calm-{}.toml", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&file, calm)
            .map_err(|e| refused(format!("cannot write {}: {e}", file.display())))?;
        Ok(file)
    }
}

/// The host clock, read around a run and never inside one.
///
/// §5.5 bans the host clock because a run that read it would stop reproducing. This reading
/// cannot reach a run: it times a whole `bohr run` process from outside, for `costs` to compare
/// two scenarios by what a person waits for, and it is reported as a measurement of this host.
#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

/// When a result was written, in seconds since the epoch: a label on a file of measurements of
/// this host, read by a person and never by a run.
#[allow(clippy::disallowed_methods)]
fn written_at() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Virtual time as a person reads it: seconds to one place.
fn seconds(ns: u64) -> String {
    format!("{}.{} s", ns / 1_000_000_000, ns / 100_000_000 % 10)
}

/// The program a scenario runs: the first word of its argv.
pub(crate) fn program_of(scenario: &Path) -> Result<String, CliError> {
    let text = std::fs::read_to_string(scenario)
        .map_err(|e| refused(format!("cannot read {}: {e}", scenario.display())))?;
    let table: toml::Table =
        toml::from_str(&text).map_err(|e| refused(format!("{}: {e}", scenario.display())))?;
    let cluster = table.get("cluster").and_then(toml::Value::as_table);
    let first = cluster
        .and_then(|c| c.get("argv"))
        .and_then(toml::Value::as_array)
        .and_then(|argv| argv.first())
        .or_else(|| {
            cluster
                .and_then(|c| c.get("programs"))
                .and_then(toml::Value::as_array)
                .and_then(|p| p.first())
                .and_then(toml::Value::as_array)
                .and_then(|argv| argv.first())
        })
        .and_then(toml::Value::as_str);
    first.map(str::to_owned).ok_or_else(|| {
        refused(format!(
            "{} names no program: give one after `--`, or an argv in [cluster]",
            scenario.display()
        ))
    })
}

/// What one check came to.
#[derive(Clone, Debug)]
pub(crate) struct Verdict {
    pub(crate) name: String,
    /// Whether the bound held; `None` where the check was not carried out.
    pub(crate) holds: Option<bool>,
    /// The measurement and the bound, in one line.
    pub(crate) line: String,
    /// What a reader needs next when it did not hold.
    pub(crate) detail: Vec<String>,
    /// What a reader is shown whether it held or not: the measurements behind the line.
    pub(crate) notes: Vec<String>,
    pub(crate) measured: serde_json::Value,
    pub(crate) bound: serde_json::Value,
}

impl Verdict {
    fn new(name: &str, holds: bool, line: String) -> Self {
        Self {
            name: name.to_owned(),
            holds: Some(holds),
            line,
            detail: Vec::new(),
            notes: Vec::new(),
            measured: serde_json::Value::Null,
            bound: serde_json::Value::Null,
        }
    }

    fn skipped(name: &str, why: &str) -> Self {
        Self { holds: None, ..Self::new(name, false, why.to_owned()) }
    }

    fn with(mut self, measured: serde_json::Value, bound: serde_json::Value) -> Self {
        self.measured = measured;
        self.bound = bound;
        self
    }
}

/// A digest as a reader tells two apart: the first twelve hex digits.
fn short(digest: &str) -> String {
    let hex = digest.rsplit(':').next().unwrap_or(digest);
    format!("{}…", &hex[..12.min(hex.len())])
}

fn seeds_of((from, to): (u64, u64)) -> Vec<u64> {
    (from..=to).collect()
}

/// Seeds as a reader scans them: the first few, then how many more.
fn listed(seeds: &[u64]) -> String {
    if seeds.is_empty() {
        return "—".to_owned();
    }
    let mut out: Vec<String> = seeds.iter().take(8).map(u64::to_string).collect();
    if seeds.len() > 8 {
        out.push(format!("and {} more", seeds.len() - 8));
    }
    out.join(" ")
}

/// Runs that did not come to anything, as the first one's own words.
fn incomplete(ran: &[Ran]) -> Option<String> {
    let lost: Vec<&Ran> = ran.iter().filter(|r| r.facts.is_none()).collect();
    lost.first().map(|first| {
        format!("{} did not complete; seed {}: {}", lost.len(), first.seed, first.said)
    })
}

/// How a check is to be carried out.
pub(crate) struct Options {
    /// Fewer repeats, and nothing that sweeps a seed range.
    pub(crate) quick: bool,
}

/// Carries out one check.
pub(crate) fn carry_out(
    check: &Check,
    subject: &Subject,
    program: &str,
    runner: &Runner,
    options: &Options,
) -> Verdict {
    let name = check.name;
    let scenario = check.scenario.as_path();
    let sweeping = matches!(
        check.kind,
        Kind::Clean { .. }
            | Kind::Adversity { .. }
            | Kind::Finds { .. }
            | Kind::NeverFinds { .. }
            | Kind::Costs { .. }
            | Kind::Performs(_)
            | Kind::WorkersAgree { .. }
    );
    if options.quick && sweeping {
        return Verdict::skipped(name, "skipped by --quick");
    }
    match &check.kind {
        Kind::Works { seed, min_events, min_virtual_s, probe_answers } => works(
            name,
            runner,
            scenario,
            program,
            *seed,
            *min_events,
            *min_virtual_s,
            *probe_answers,
        ),
        Kind::Repeats { seed, times } => {
            let times = if options.quick { (*times).min(10) } else { *times };
            repeats(name, runner, scenario, program, *seed, times)
        }
        Kind::Replays { seed } => replays(name, runner, scenario, program, *seed),
        Kind::Clean { seeds } => clean(name, runner, scenario, program, *seeds),
        Kind::Adversity { seeds, at_least } => {
            adversity(name, runner, scenario, program, *seeds, *at_least)
        }
        Kind::Finds { seeds, code, at_least } => {
            let ran = runner.many(scenario, program, &seeds_of(*seeds));
            finds(name, &ran, code, *at_least)
        }
        Kind::NeverFinds { seeds } => {
            let ran = runner.many(scenario, program, &seeds_of(*seeds));
            never_finds(name, &ran)
        }
        Kind::Walls { seed, allow } => walls(name, runner, scenario, program, *seed, allow),
        Kind::Costs { seed, against, metric, max_ratio } => {
            costs(name, runner, scenario, against, program, *seed, *metric, *max_ratio)
        }
        Kind::Performs(spec) => performs(name, runner, scenario, spec),
        Kind::WorkersAgree { runs, jobs } => {
            workers_agree(name, runner, scenario, program, *runs, *jobs)
        }
        Kind::RefusesChanged => refuses_changed(name, subject, runner),
    }
}

#[allow(clippy::too_many_arguments)]
fn works(
    name: &str,
    runner: &Runner,
    scenario: &Path,
    program: &str,
    seed: u64,
    min_events: u64,
    min_virtual_s: Milli,
    probe_answers: Option<Milli>,
) -> Verdict {
    // Without faults: whether the program runs at all is a property of the program, and a seed
    // that kills it while it starts measures the fault. What faults reach is `adversity`'s.
    let calm = match runner.without_faults(scenario) {
        Ok(calm) => calm,
        Err(e) => return Verdict::new(name, false, e.to_string()),
    };
    let ran = runner.one(&calm, program, seed, 0, &[]);
    let _ = std::fs::remove_file(&calm);
    let Some(facts) = ran.facts else {
        return Verdict::new(name, false, format!("seed {seed} did not complete: {}", ran.said));
    };
    let reached = facts.virtual_ns >= min_virtual_s.0.saturating_mul(1_000_000);
    let mut line = format!("{} virtual · {} events", seconds(facts.virtual_ns), facts.events);
    let mut holds = facts.events >= min_events && reached;
    let mut short_of = Vec::new();
    if facts.events < min_events {
        short_of.push(format!("{} events, at least {min_events}", facts.events));
    }
    if !reached {
        short_of.push(format!(
            "{} virtual, at least {min_virtual_s} s: the program stopped early",
            seconds(facts.virtual_ns)
        ));
    }
    if let Some(wanted) = probe_answers {
        line.push_str(&format!(
            " · probe answered {} of {}",
            facts.probe_answered, facts.probe_asked
        ));
        let share = Milli::of(facts.probe_answered, facts.probe_asked);
        if share < wanted {
            holds = false;
            short_of.push(format!(
                "probe answered {}%, at least {}%",
                share.percent(),
                wanted.percent()
            ));
        }
    }
    // A finding with nothing done to the program is a program that does not work, whatever it
    // counted on the way: a node that fails on a full file at its tenth second has events and
    // virtual time to spare.
    if facts.found() {
        holds = false;
        short_of.push(format!("ended {} with no fault: {}", facts.outcome, clip(&facts.detail)));
    }
    let mut verdict = Verdict::new(name, holds, line).with(
        serde_json::json!({ "events": facts.events, "virtual_ns": facts.virtual_ns,
            "probe_asked": facts.probe_asked, "probe_answered": facts.probe_answered }),
        serde_json::json!({ "min_events": min_events, "min_virtual_s": min_virtual_s.to_string(),
            "probe_answers": probe_answers.map(|p| p.to_string()) }),
    );
    verdict.detail = short_of;
    verdict
}

fn repeats(
    name: &str,
    runner: &Runner,
    scenario: &Path,
    program: &str,
    seed: u64,
    times: u64,
) -> Verdict {
    let seeds = vec![seed; usize::try_from(times).unwrap_or(usize::MAX)];
    let ran = runner.many(scenario, program, &seeds);
    let mut digests: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for facts in ran.iter().filter_map(|r| r.facts.as_ref()) {
        *digests.entry(facts.digest.as_str()).or_default() += 1;
    }
    let lost = incomplete(&ran);
    let holds = digests.len() == 1 && lost.is_none();
    let line = match digests.keys().next() {
        Some(only) if digests.len() == 1 => {
            format!("{times} runs of seed {seed} · 1 digest ({})", short(only))
        }
        _ => format!("{times} runs of seed {seed} · {} digests", digests.len()),
    };
    let mut verdict = Verdict::new(name, holds, line).with(
        serde_json::json!({ "runs": times, "digests": digests.len() }),
        serde_json::json!({ "digests": 1 }),
    );
    if digests.len() > 1 {
        verdict.detail = digests.iter().map(|(d, n)| format!("{} × {n}", short(d))).collect();
    }
    verdict.detail.extend(lost);
    verdict
}

fn replays(name: &str, runner: &Runner, scenario: &Path, program: &str, seed: u64) -> Verdict {
    let bundle =
        runner.scratch.join(format!("replay-{}.bohr", NEXT.fetch_add(1, Ordering::Relaxed)));
    let ran = runner.one(scenario, program, seed, 0, &["--save".to_owned(), absolute(&bundle)]);
    let Some(facts) = ran.facts else {
        return Verdict::new(name, false, format!("seed {seed} did not complete: {}", ran.said));
    };
    let replayed = Command::new(&runner.me)
        .args(["replay", "--verify-only"])
        .arg(&bundle)
        .current_dir(&runner.cwd)
        .output();
    let _ = std::fs::remove_file(&bundle);
    let (holds, said) = match &replayed {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(if out.status.success() { &out.stdout } else { &out.stderr })
                .lines()
                .last()
                .unwrap_or("")
                .to_owned(),
        ),
        Err(e) => (false, e.to_string()),
    };
    let line = if holds {
        format!("seed {seed} saved and replayed to {}", short(&facts.digest))
    } else {
        format!("seed {seed} saved and did not replay to {}", short(&facts.digest))
    };
    let mut verdict = Verdict::new(name, holds, line).with(
        serde_json::json!({ "digest": facts.digest }),
        serde_json::json!({ "replays": true }),
    );
    if !holds {
        verdict.detail.push(said);
    }
    verdict
}

fn clean(
    name: &str,
    runner: &Runner,
    scenario: &Path,
    program: &str,
    seeds: (u64, u64),
) -> Verdict {
    let calm = match runner.without_faults(scenario) {
        Ok(calm) => calm,
        Err(e) => return Verdict::new(name, false, e.to_string()),
    };
    let ran = runner.many(&calm, program, &seeds_of(seeds));
    let _ = std::fs::remove_file(&calm);
    let found: Vec<&Ran> =
        ran.iter().filter(|r| r.facts.as_ref().is_some_and(Facts::found)).collect();
    let lost = incomplete(&ran);
    let line = format!("{} seeds without faults · {} findings", ran.len(), found.len());
    let mut verdict = Verdict::new(name, found.is_empty() && lost.is_none(), line).with(
        serde_json::json!({ "seeds": ran.len(), "findings": found.len() }),
        serde_json::json!({ "findings": 0 }),
    );
    for r in found.iter().take(3) {
        if let Some(f) = &r.facts {
            verdict.detail.push(format!("seed {}: {} {}", r.seed, f.outcome, clip(&f.detail)));
        }
    }
    verdict.detail.extend(lost);
    verdict
}

fn adversity(
    name: &str,
    runner: &Runner,
    scenario: &Path,
    program: &str,
    seeds: (u64, u64),
    at_least: Milli,
) -> Verdict {
    let ran = runner.many(scenario, program, &seeds_of(seeds));
    let met = ran.iter().filter(|r| r.facts.as_ref().is_some_and(|f| f.adversity > 0)).count();
    let share = Milli::of(met as u64, ran.len() as u64);
    let line = format!(
        "{met} of {} seeds met a fault ({}%, at least {}%)",
        ran.len(),
        share.percent(),
        at_least.percent()
    );
    let quiet: Vec<u64> = ran
        .iter()
        .filter(|r| r.facts.as_ref().is_some_and(|f| f.adversity == 0))
        .map(|r| r.seed)
        .collect();
    let mut verdict = Verdict::new(name, share >= at_least, line).with(
        serde_json::json!({ "seeds": ran.len(), "met": met }),
        serde_json::json!({ "at_least": at_least.to_string() }),
    );
    if share < at_least {
        verdict.detail.push(format!("seeds no fault reached: {}", listed(&quiet)));
    }
    verdict.detail.extend(incomplete(&ran));
    verdict
}

/// Whether a sweep found `code` on at least `at_least` seeds.
pub(crate) fn finds(name: &str, ran: &[Ran], code: &str, at_least: u64) -> Verdict {
    let hits: Vec<u64> = ran
        .iter()
        .filter(|r| r.facts.as_ref().is_some_and(|f| f.names(code)))
        .map(|r| r.seed)
        .collect();
    let other = ran
        .iter()
        .filter(|r| r.facts.as_ref().is_some_and(|f| f.found() && !f.names(code)))
        .count();
    let lost = ran.iter().filter(|r| r.facts.is_none()).count();
    let holds = hits.len() as u64 >= at_least;
    let line = format!(
        "{} of {} seeds found {code} (at least {at_least}) · {other} other findings · {lost} did not complete",
        hits.len(),
        ran.len()
    );
    let mut verdict = Verdict::new(name, holds, line).with(
        serde_json::json!({ "seeds": ran.len(), "found": hits.len(), "other": other, "incomplete": lost }),
        serde_json::json!({ "code": code, "at_least": at_least }),
    );
    verdict.detail.push(format!("first seeds with it: {}", listed(&hits)));
    verdict.detail.extend(incomplete(ran));
    verdict
}

/// Whether a sweep found nothing at all.
pub(crate) fn never_finds(name: &str, ran: &[Ran]) -> Verdict {
    let found: Vec<&Ran> =
        ran.iter().filter(|r| r.facts.as_ref().is_some_and(Facts::found)).collect();
    let lost = incomplete(ran);
    let line = format!("{} seeds · {} findings", ran.len(), found.len());
    let mut verdict = Verdict::new(name, found.is_empty() && lost.is_none(), line).with(
        serde_json::json!({ "seeds": ran.len(), "findings": found.len() }),
        serde_json::json!({ "findings": 0 }),
    );
    for r in found.iter().take(3) {
        if let Some(f) = &r.facts {
            verdict.detail.push(format!("seed {}: {} {}", r.seed, f.outcome, clip(&f.detail)));
        }
    }
    verdict.detail.extend(lost);
    verdict
}

fn clip(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() > 100 {
        format!("{}…", line.chars().take(100).collect::<String>())
    } else {
        line.to_owned()
    }
}

/// Reads `bohr survey`'s table: each row's name and the kind of wall it is.
fn surveyed(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let nr = words.next()?;
            if !nr.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let name = words.next()?.to_owned();
            let calls = words.next()?;
            if !calls.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some((name, words.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

fn walls(
    name: &str,
    runner: &Runner,
    scenario: &Path,
    program: &str,
    seed: u64,
    allow: &[String],
) -> Verdict {
    // Surveyed without faults: a node killed early is a survey of the first second of its life.
    let calm = match runner.without_faults(scenario) {
        Ok(calm) => calm,
        Err(e) => return Verdict::new(name, false, e.to_string()),
    };
    let out = Command::new(&runner.me)
        .args(["survey", "--seed", &seed.to_string(), "--config", &absolute(&calm), "--", program])
        .current_dir(&runner.cwd)
        .output();
    let _ = std::fs::remove_file(&calm);
    let out = match out {
        Ok(out) => out,
        Err(e) => return Verdict::new(name, false, format!("cannot start the survey: {e}")),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("did not run to completion") {
        return Verdict::new(
            name,
            false,
            "the guest did not run to completion, so this is not its surface".to_owned(),
        );
    }
    if !stdout.starts_with("survey") {
        let said = String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("").to_owned();
        return Verdict::new(name, false, format!("the survey did not complete: {said}"));
    }
    let found = surveyed(&stdout);
    let (allowed, walls): (Vec<_>, Vec<_>) = found.iter().partition(|(n, _)| allow.contains(n));
    let named = |list: &[&(String, String)]| {
        list.iter().map(|(n, w)| format!("{n}: {w}")).collect::<Vec<_>>().join(", ")
    };
    let line = match (allowed.is_empty(), walls.is_empty()) {
        (true, true) => "every call the program made was answered".to_owned(),
        (false, true) => format!("only the walls allowed for: {}", named(&allowed)),
        (_, false) => format!("{} walls not allowed for", walls.len()),
    };
    let mut verdict = Verdict::new(name, walls.is_empty(), line).with(
        serde_json::json!({ "walls": found.iter().map(|(n, _)| n).collect::<Vec<_>>() }),
        serde_json::json!({ "allow": allow }),
    );
    verdict.detail = walls.iter().map(|(n, w)| format!("{n}: {w}")).collect();
    verdict
}

#[allow(clippy::too_many_arguments)]
fn costs(
    name: &str,
    runner: &Runner,
    scenario: &Path,
    against: &Path,
    program: &str,
    seed: u64,
    metric: Metric,
    max_ratio: Milli,
) -> Verdict {
    // Alternately and best of three, so a host that slowed down for one run slows both sides.
    let mut best = [(u64::MAX, Duration::MAX); 2];
    for _ in 0..3 {
        for (side, file) in [scenario, against].iter().enumerate() {
            let ran = runner.one(file, program, seed, 0, &[]);
            let Some(facts) = ran.facts else {
                return Verdict::new(
                    name,
                    false,
                    format!("{} did not complete: {}", file.display(), ran.said),
                );
            };
            best[side] = (facts.events.min(best[side].0), ran.wall.min(best[side].1));
        }
    }
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let (ours, theirs, unit) = match metric {
        Metric::Events => (best[0].0, best[1].0, "events"),
        Metric::Wall => (ms(best[0].1), ms(best[1].1), "ms"),
    };
    // A comparison that measured nothing compares nothing: a ratio against zero would hold every
    // bound and say so.
    if theirs == 0 {
        return Verdict::new(
            name,
            false,
            format!("{} measured 0 {unit}: there is nothing to compare against", against.display()),
        );
    }
    let ratio = Milli::of(ours, theirs);
    let line = format!(
        "×{ratio} by {} ({ours} {unit} against {theirs}), at most ×{max_ratio}",
        if metric == Metric::Events { "events" } else { "wall" }
    );
    let against_name =
        against.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Verdict::new(name, ratio <= max_ratio, line).with(
        serde_json::json!({ "ratio": ratio.to_string(), "ours": ours, "against": theirs, "unit": unit }),
        serde_json::json!({ "max_ratio": max_ratio.to_string(), "against": against_name }),
    )
}

fn workers_agree(
    name: &str,
    runner: &Runner,
    scenario: &Path,
    program: &str,
    runs: u64,
    jobs: usize,
) -> Verdict {
    let mut summaries = Vec::new();
    for j in [1, jobs] {
        let at = NEXT.fetch_add(1, Ordering::Relaxed);
        let summary = runner.scratch.join(format!("summary-{at}.json"));
        let findings = runner.scratch.join(format!("findings-{at}"));
        let status = Command::new(&runner.me)
            .args([
                "explore",
                "--runs",
                &runs.to_string(),
                "-j",
                &j.to_string(),
                "--config",
                &absolute(scenario),
            ])
            .arg("--summary")
            .arg(&summary)
            .arg("--findings")
            .arg(&findings)
            .args(["--", program])
            .current_dir(&runner.cwd)
            .output();
        let text = std::fs::read(&summary).ok();
        let _ = std::fs::remove_file(&summary);
        let _ = std::fs::remove_dir_all(&findings);
        let Some(text) = text else {
            let said = status.map_or_else(
                |e| e.to_string(),
                |o| String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("").to_owned(),
            );
            return Verdict::new(
                name,
                false,
                format!("the campaign at -j {j} wrote no summary: {said}"),
            );
        };
        summaries.push(text);
    }
    let same = summaries[0] == summaries[1];
    let line = if same {
        format!("{runs} runs · -j 1 and -j {jobs} wrote one summary, byte for byte")
    } else {
        format!("{runs} runs · -j 1 and -j {jobs} wrote different summaries")
    };
    Verdict::new(name, same, line).with(
        serde_json::json!({ "runs": runs, "jobs": jobs, "same": same }),
        serde_json::json!({ "same": true }),
    )
}

/// Nanoseconds as microseconds to two places.
fn micros(ns: u64) -> String {
    format!("{}.{:02}", ns / 1000, ns % 1000 / 10)
}

/// A ratio in thousandths, to one place; below a tenth, as less than one.
fn tenths(ratio: Milli) -> String {
    if ratio.0 < 100 {
        return "<0.1".to_owned();
    }
    format!("{}.{}", ratio.0 / 1000, ratio.0 % 1000 / 100)
}

/// The fastest of `times` runs of a scenario, with what that run came to.
///
/// A run that found anything is no measurement: a workload the supervisor answered wrongly would
/// otherwise read as a fast one.
fn fastest(
    runner: &Runner,
    scenario: &Path,
    program: &str,
    times: u64,
) -> Result<(u64, Facts), String> {
    let mut best: Option<(u64, Facts)> = None;
    for _ in 0..times {
        let ran = runner.one(scenario, program, 1, 0, &[]);
        let Some(facts) = ran.facts else {
            return Err(format!("did not complete: {}", ran.said));
        };
        if facts.found() {
            return Err(format!("ended {}: {}", facts.outcome, clip(&facts.detail)));
        }
        let wall = u64::try_from(ran.wall.as_nanos()).unwrap_or(u64::MAX);
        if best.as_ref().is_none_or(|(b, _)| wall < *b) {
            best = Some((wall, facts));
        }
    }
    best.ok_or_else(|| "no attempt was made".to_owned())
}

/// The fastest of `times` native runs of an argv, in a directory of its own.
fn fastest_native(argv: &[String], cwd: &Path, times: u64) -> Result<u64, String> {
    let mut best = u64::MAX;
    for _ in 0..times {
        let started = now();
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("cannot run {} natively: {e}", argv[0]))?;
        let wall = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if !status.success() {
            return Err(format!("{} failed natively: {status}", argv.join(" ")));
        }
        best = best.min(wall);
    }
    Ok(best)
}

/// What one operation of a golden workload costs: time, the host's time for the same, the
/// supervisor's stops, events and log, and the memory it held.
///
/// **Per operation, with startup taken out.** The workload is run again with no operations, and
/// natively both ways, so what remains is what the operations cost and not what starting a process
/// does; a short workload's figure is otherwise mostly its start.
fn performs(name: &str, runner: &Runner, scenario: &Path, spec: &Performs) -> Verdict {
    let program = match program_of(scenario) {
        Ok(program) => program,
        Err(e) => return Verdict::new(name, false, e.to_string()),
    };
    let zero = runner.scratch.join(format!("zero-{}.toml", NEXT.fetch_add(1, Ordering::Relaxed)));
    if let Err(e) = std::fs::write(&zero, &spec.zero) {
        return Verdict::new(name, false, format!("cannot write {}: {e}", zero.display()));
    }
    let traced = fastest(runner, scenario, &program, spec.best_of)
        .and_then(|full| Ok((full, fastest(runner, &zero, &program, spec.best_of)?)));
    let _ = std::fs::remove_file(&zero);
    let ((wall, facts), (wall0, facts0)) = match traced {
        Ok(both) => both,
        Err(why) => return Verdict::new(name, false, why),
    };
    let ops = spec.ops.max(1);
    let per_op = wall.saturating_sub(wall0) / ops;

    let native = spec.native.as_ref().map(|(argv, argv0)| {
        let dir = runner.scratch.join(format!("native-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::create_dir_all(&dir);
        // The program is named from where runs start, and the native runs start elsewhere.
        let rooted = |argv: &[String]| {
            let mut argv = argv.to_vec();
            argv[0] = absolute(&runner.cwd.join(&argv[0]));
            argv
        };
        let both = fastest_native(&rooted(argv), &dir, spec.best_of)
            .and_then(|n| Ok((n, fastest_native(&rooted(argv0), &dir, spec.best_of)?)));
        let _ = std::fs::remove_dir_all(&dir);
        both.map(|(n, n0)| n.saturating_sub(n0) / ops)
    });
    let native_per_op = match native {
        Some(Err(why)) => return Verdict::new(name, false, why),
        Some(Ok(ns)) => Some(ns),
        None => None,
    };

    let slowdown = native_per_op.map(|n| Milli::of(per_op, n.max(1)));
    let stops = Milli::of(facts.stops.saturating_sub(facts0.stops), ops);
    let events = Milli::of(facts.events.saturating_sub(facts0.events), ops);
    let log_per_op = facts.log_bytes.saturating_sub(facts0.log_bytes) / ops;
    let rss_mib = facts.peak_rss_kib / 1024;

    let mut misses = Vec::new();
    if let Some(bound) = spec.max_us_per_op
        && per_op > bound.0
    {
        misses.push(format!("{} µs/op, at most {bound}", micros(per_op)));
    }
    if let (Some(bound), Some(got)) = (spec.max_slowdown, slowdown)
        && got > bound
    {
        misses.push(format!("×{} native, at most ×{bound}", tenths(got)));
    }
    if let Some(bound) = spec.max_rss_mib
        && rss_mib > bound
    {
        misses.push(format!("{rss_mib} MiB held, at most {bound}"));
    }
    if let Some(bound) = spec.max_stops_per_op
        && stops > bound
    {
        misses.push(format!("{stops} stops/op, at most {bound}"));
    }

    let line = format!(
        "{} µs/op{} · bohr {rss_mib} MiB peak",
        micros(per_op),
        match (slowdown, native_per_op) {
            (Some(times), Some(n)) => format!(" · ×{} native ({} µs/op)", tenths(times), micros(n)),
            _ => " · no native form".to_owned(),
        }
    );
    let mut verdict = Verdict::new(name, misses.is_empty(), line).with(
        serde_json::json!({
            "ops": spec.ops, "ns_per_op": per_op, "native_ns_per_op": native_per_op,
            "slowdown": slowdown.map(|s| s.to_string()), "stops_per_op": stops.to_string(),
            "events_per_op": events.to_string(), "log_bytes_per_op": log_per_op,
            "startup_ns": wall0, "peak_rss_kib": facts.peak_rss_kib,
            "cpu_user_ms": facts.cpu_user_ms, "cpu_system_ms": facts.cpu_system_ms,
            "guest_cpu_ms": facts.guest_cpu_ms,
        }),
        serde_json::json!({
            "max_us_per_op": spec.max_us_per_op.map(|b| b.to_string()),
            "max_slowdown": spec.max_slowdown.map(|b| b.to_string()),
            "max_rss_mib": spec.max_rss_mib,
            "max_stops_per_op": spec.max_stops_per_op.map(|b| b.to_string()),
        }),
    );
    verdict.notes.push(format!(
        "{stops} stops/op · {events} events/op · {log_per_op} B of log/op · startup {} ms",
        wall0 / 1_000_000
    ));
    verdict.notes.push(format!(
        "cpu: bohr {} ms user, {} ms system · guest {} ms",
        facts.cpu_user_ms, facts.cpu_system_ms, facts.guest_cpu_ms
    ));
    verdict.detail = misses;
    verdict
}

fn refuses_changed(name: &str, subject: &Subject, runner: &Runner) -> Verdict {
    let Some(pack) = &subject.pack else {
        return Verdict::new(name, false, "only a pack pins a binary".to_owned());
    };
    let changed = runner.scratch.join(format!("changed-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
    let copied = std::fs::read(&pack.binary).and_then(|mut bytes| {
        bytes.push(0);
        std::fs::write(&changed, bytes)
    });
    if let Err(e) = copied {
        return Verdict::new(name, false, format!("cannot copy {}: {e}", pack.binary.display()));
    }
    let answer = crate::targets::binary_for(pack, Some(&changed));
    let _ = std::fs::remove_file(&changed);
    match answer {
        Err(e) if e.to_string().contains("is not the program the pack names") => {
            Verdict::new(name, true, "a copy one byte longer is refused by its digest".to_owned())
        }
        Err(e) => Verdict::new(name, false, format!("refused, but not by its digest: {e}")),
        Ok(_) => Verdict::new(
            name,
            false,
            "a copy one byte longer was taken for the pack's binary".to_owned(),
        ),
    }
}

/// Where results are written and what is printed.
pub(crate) struct Request {
    pub(crate) subjects: Vec<Subject>,
    pub(crate) only: Vec<String>,
    pub(crate) options: Options,
    pub(crate) plan: bool,
    pub(crate) jobs: usize,
    pub(crate) out: PathBuf,
}

/// Carries out every check of every subject and prints one block per subject.
///
/// Under `--json` nothing is printed, and one `bohr.check` object carries every subject: its plan
/// with `--plan`, otherwise the account written to its file with each result's detail and notes.
pub(crate) fn check(request: &Request) -> Result<u8, CliError> {
    for name in &request.only {
        if !crate::subjects::CHECKS.contains(&name.as_str()) {
            return Err(refused(format!(
                "--only {name}: no such check; the checks are {}",
                crate::subjects::CHECKS.join(", ")
            )));
        }
    }
    let text = !crate::json::on();
    let mut worst = crate::exit::CLEAN;
    let mut accounts: Vec<serde_json::Value> = Vec::new();
    for subject in &request.subjects {
        if text {
            println!("{} · {}", subject.name, subject.about);
        }
        let notes: std::collections::BTreeMap<&str, &str> = subject
            .groups
            .iter()
            .filter_map(|g| Some((g.label.as_deref()?, g.note.as_deref()?)))
            .collect();
        let checks: Vec<(&Option<String>, &Check)> = subject
            .groups
            .iter()
            .flat_map(|g| g.checks.iter().map(move |c| (&g.label, c)))
            .filter(|(_, c)| request.only.is_empty() || request.only.iter().any(|o| o == c.name))
            .collect();
        if !text && (checks.is_empty() || request.plan) {
            accounts.push(planned(subject, &checks));
            continue;
        }
        if checks.is_empty() {
            println!("\n  no checks declared\n");
            continue;
        }
        if request.plan {
            let width = checks.iter().filter_map(|(l, _)| l.as_ref().map(String::len)).max();
            println!();
            for (label, c) in &checks {
                let label = match width {
                    Some(width) => format!("{:<width$} ", label.as_deref().unwrap_or("")),
                    None => String::new(),
                };
                println!("  {:<16} {label}{}", c.name, describe(c, &subject.cwd));
            }
            println!();
            continue;
        }
        // A pack whose binary is refused is reported and passed over, not the end of the command:
        // the subjects after it are no less checkable for it.
        let program = match &subject.pack {
            Some(pack) => match crate::targets::binary_for(pack, None) {
                Ok(binary) => absolute(&binary),
                Err(e) => {
                    if text {
                        println!("\n  not checked: {e}\n");
                    } else {
                        accounts.push(serde_json::json!({
                            "subject": subject.name, "not_checked": e.to_string(),
                        }));
                    }
                    worst = worst.max(crate::exit::USAGE);
                    continue;
                }
            },
            None => String::new(),
        };
        let scratch = request.out.join(format!("{}.runs", subject.name));
        let runner = Runner::new(&subject.cwd, &scratch, request.jobs)?;
        let mut verdicts = Vec::new();
        let mut label_now: Option<&String> = None;
        if text {
            println!();
        }
        for (label, c) in &checks {
            if label.as_ref() != label_now {
                label_now = label.as_ref();
                if let Some(label) = label_now
                    && text
                {
                    match notes.get(label.as_str()) {
                        Some(note) => println!("  {label} — {note}"),
                        None => println!("  {label}"),
                    }
                }
            }
            // A generated scenario is written beside the runs, so every check reads a file as it
            // does for any other subject.
            let materialised;
            let c: &Check = match &c.generated {
                Some(text) => {
                    let stem: String = label
                        .as_deref()
                        .unwrap_or(c.name)
                        .chars()
                        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                        .collect();
                    let file = scratch.join(format!("{stem}.toml"));
                    std::fs::write(&file, text)
                        .map_err(|e| refused(format!("cannot write {}: {e}", file.display())))?;
                    materialised = Check { scenario: file, generated: None, ..(*c).clone() };
                    &materialised
                }
                None => c,
            };
            let program =
                if program.is_empty() { program_of(&c.scenario)? } else { program.clone() };
            let mut verdict = carry_out(c, subject, &program, &runner, &request.options);
            if let Some(label) = label {
                verdict.name = format!("{label} {}", c.name);
            }
            if text {
                print_verdict(c.name, &verdict, label.is_some());
            }
            verdicts.push(verdict);
        }
        let _ = std::fs::remove_dir_all(&scratch);
        let ran: Vec<&Verdict> = verdicts.iter().filter(|v| v.holds.is_some()).collect();
        let held = ran.iter().filter(|v| v.holds == Some(true)).count();
        let file = request.out.join(format!("{}.json", subject.name));
        let account = write_results(&file, subject, &verdicts)?;
        if text {
            println!("\n  {held} of {} hold · results in {}\n", ran.len(), file.display());
        } else {
            accounts.push(with_what_was_printed(account, &verdicts, &file, held, ran.len()));
        }
        if held < ran.len() {
            worst = worst.max(crate::exit::FINDING);
        }
    }
    if !text {
        crate::json::emit(
            "check",
            serde_json::json!({ "plan": request.plan, "subjects": accounts }),
        );
    }
    Ok(worst)
}

/// A subject's plan as data: each check, the label it sits under and the scenario it runs.
fn planned(subject: &Subject, checks: &[(&Option<String>, &Check)]) -> serde_json::Value {
    let checks: Vec<serde_json::Value> = checks
        .iter()
        .map(|(label, c)| {
            serde_json::json!({
                "name": c.name,
                "label": label,
                "scenario": c.generated.is_none().then(|| c.scenario.display().to_string()),
                "generated": c.generated.is_some(),
                "description": describe(c, &subject.cwd),
            })
        })
        .collect();
    serde_json::json!({ "subject": subject.name, "about": subject.about, "checks": checks })
}

/// The account a subject's file holds, with what the text shows and the file does not: the file's
/// path, how many checks held of how many ran, and each result's detail and notes.
fn with_what_was_printed(
    mut account: serde_json::Value,
    verdicts: &[Verdict],
    file: &Path,
    held: usize,
    ran: usize,
) -> serde_json::Value {
    if let Some(results) = account.get_mut("results").and_then(serde_json::Value::as_array_mut) {
        for (result, verdict) in results.iter_mut().zip(verdicts) {
            if let serde_json::Value::Object(fields) = result {
                fields.insert("detail".to_owned(), serde_json::json!(verdict.detail));
                fields.insert("notes".to_owned(), serde_json::json!(verdict.notes));
            }
        }
    }
    if let serde_json::Value::Object(fields) = &mut account {
        fields.insert("file".to_owned(), serde_json::json!(file.display().to_string()));
        fields.insert("held".to_owned(), serde_json::json!(held));
        fields.insert("ran".to_owned(), serde_json::json!(ran));
    }
    account
}

fn describe(check: &Check, cwd: &Path) -> String {
    let scenario = if check.generated.is_some() {
        "generated".to_owned()
    } else {
        check.scenario.strip_prefix(cwd).unwrap_or(&check.scenario).display().to_string()
    };
    let params = match &check.kind {
        Kind::Works { seed, min_events, min_virtual_s, probe_answers } => format!(
            "seed {seed}, at least {min_events} events and {min_virtual_s} s virtual{}",
            probe_answers.map_or_else(String::new, |p| format!(", probe answers {}%", p.percent()))
        ),
        Kind::Repeats { seed, times } => format!("seed {seed} × {times}, one digest"),
        Kind::Replays { seed } => format!("seed {seed} saved and replayed"),
        Kind::Clean { seeds } => {
            format!("seeds {}..{} without faults, no findings", seeds.0, seeds.1)
        }
        Kind::Adversity { seeds, at_least } => {
            format!("seeds {}..{}, at least {}% met a fault", seeds.0, seeds.1, at_least.percent())
        }
        Kind::Finds { seeds, code, at_least } => {
            format!("seeds {}..{}, {code} on at least {at_least}", seeds.0, seeds.1)
        }
        Kind::NeverFinds { seeds } => format!("seeds {}..{}, no findings", seeds.0, seeds.1),
        Kind::Walls { seed, allow } => format!(
            "seed {seed}, walls allowed: {}",
            if allow.is_empty() { "none".to_owned() } else { allow.join(", ") }
        ),
        Kind::Costs { against, metric, max_ratio, .. } => format!(
            "at most ×{max_ratio} of {} by {}",
            against.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            if *metric == Metric::Events { "events" } else { "wall" }
        ),
        Kind::Performs(spec) => {
            let mut bounds = Vec::new();
            if let Some(us) = spec.max_us_per_op {
                bounds.push(format!("at most {us} µs/op"));
            }
            if let Some(times) = spec.max_slowdown {
                bounds.push(format!("at most ×{times} native"));
            }
            if let Some(mib) = spec.max_rss_mib {
                bounds.push(format!("at most {mib} MiB"));
            }
            if let Some(stops) = spec.max_stops_per_op {
                bounds.push(format!("at most {stops} stops/op"));
            }
            format!(
                "{} operations, best of {}{}",
                spec.ops,
                spec.best_of,
                if bounds.is_empty() { String::new() } else { format!(", {}", bounds.join(", ")) }
            )
        }
        Kind::WorkersAgree { runs, jobs } => format!("{runs} runs, -j 1 against -j {jobs}"),
        Kind::RefusesChanged => "a changed binary is refused".to_owned(),
    };
    format!("{params} · {scenario}")
}

fn print_verdict(name: &str, verdict: &Verdict, indented: bool) {
    let mark = match verdict.holds {
        Some(true) => "ok",
        Some(false) => "FAIL",
        None => "—",
    };
    let pad = if indented { "    " } else { "  " };
    println!("{pad}{name:<16} {mark:<5} {}", verdict.line);
    for line in &verdict.notes {
        println!("{pad}{:<16} {:<5} {line}", "", "");
    }
    if verdict.holds == Some(false) {
        for line in &verdict.detail {
            println!("{pad}{:<16} {:<5} {line}", "", "");
        }
    }
}

/// Writes a subject's results to `file`, and returns the account written.
fn write_results(
    file: &Path,
    subject: &Subject,
    verdicts: &[Verdict],
) -> Result<serde_json::Value, CliError> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| refused(format!("cannot use {}: {e}", dir.display())))?;
    }
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map_or_else(|_| "unknown".to_owned(), |h| h.trim().to_owned());
    let at = written_at();
    let results: Vec<serde_json::Value> = verdicts
        .iter()
        .map(|v| {
            serde_json::json!({ "subject": subject.name, "check": v.name, "holds": v.holds,
                "measured": v.measured, "bound": v.bound, "line": v.line })
        })
        .collect();
    let account = serde_json::json!({
        "subject": subject.name, "tool": env!("CARGO_PKG_VERSION"), "host": host, "at_unix": at,
        "results": results,
    });
    let mut text = serde_json::to_string_pretty(&account).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    std::fs::write(file, text)
        .map_err(|e| refused(format!("cannot write {}: {e}", file.display())))?;
    Ok(account)
}

/// What `bohr sweep` was asked for.
pub(crate) struct Sweep {
    pub(crate) scenario: PathBuf,
    pub(crate) seeds: (u64, u64),
    pub(crate) expect: Option<String>,
    pub(crate) at_least: Option<u64>,
    pub(crate) no_faults: bool,
    pub(crate) jobs: usize,
    pub(crate) program: Option<String>,
}

/// Runs each seed of a range once and says what was found on which.
pub(crate) fn sweep(request: &Sweep) -> Result<u8, CliError> {
    let cwd = std::env::current_dir().map_err(|e| refused(format!("no working directory: {e}")))?;
    let scratch = std::env::temp_dir().join(format!("bohr-sweep-{}", std::process::id()));
    let runner = Runner::new(&cwd, &scratch, request.jobs)?;
    let program = match &request.program {
        Some(p) => p.clone(),
        None => program_of(&request.scenario)?,
    };
    let scenario = if request.no_faults {
        runner.without_faults(&request.scenario)?
    } else {
        request.scenario.clone()
    };
    let ran = runner.many(&scenario, &program, &seeds_of(request.seeds));
    let _ = std::fs::remove_dir_all(&scratch);

    if crate::json::on() {
        crate::json::emit("sweep", swept(request, &program, &ran));
    } else {
        print_sweep(request, &ran);
    }
    Ok(if ran.iter().any(|r| r.facts.is_none()) {
        crate::exit::INTERNAL
    } else {
        match (&request.expect, request.at_least) {
            (Some(code), Some(at_least)) => {
                let hits =
                    ran.iter().filter(|r| r.facts.as_ref().is_some_and(|f| f.names(code))).count()
                        as u64;
                if hits >= at_least { crate::exit::CLEAN } else { crate::exit::FINDING }
            }
            _ if ran.iter().any(|r| r.facts.as_ref().is_some_and(Facts::found)) => {
                crate::exit::FINDING
            }
            _ => crate::exit::CLEAN,
        }
    })
}

/// What a sweep found on each seed, as the body of a `bohr.sweep` object.
///
/// Every seed's facts, in seed order, beside the counts the text shows; `hits` and `holds` only
/// where `--expect` and `--at-least` asked for them.
fn swept(request: &Sweep, program: &str, ran: &[Ran]) -> serde_json::Value {
    let seeds_where = |keep: &dyn Fn(&Ran) -> bool| -> Vec<u64> {
        ran.iter().filter(|r| keep(r)).map(|r| r.seed).collect()
    };
    let clean = seeds_where(&|r| r.facts.as_ref().is_some_and(|f| !f.found()));
    let found = seeds_where(&|r| r.facts.as_ref().is_some_and(Facts::found));
    let incomplete = seeds_where(&|r| r.facts.is_none());
    let hits = request
        .expect
        .as_ref()
        .map(|code| seeds_where(&|r| r.facts.as_ref().is_some_and(|f| f.names(code))));
    // Counted, not subtracted: with `--expect Clean` a clean seed is also a hit.
    let other = request.expect.as_ref().map(|code| {
        seeds_where(&|r| r.facts.as_ref().is_some_and(|f| f.found() && !f.names(code))).len()
    });
    let holds = hits.as_ref().zip(request.at_least).map(|(h, at_least)| h.len() as u64 >= at_least);
    let mut kinds: std::collections::BTreeMap<String, Vec<u64>> = std::collections::BTreeMap::new();
    for r in ran {
        if let Some(f) = r.facts.as_ref().filter(|f| f.found()) {
            let key = f.codes.first().cloned().unwrap_or_else(|| f.outcome.clone());
            kinds.entry(key).or_default().push(r.seed);
        }
    }
    let results: Vec<serde_json::Value> = ran
        .iter()
        .map(|r| {
            serde_json::json!({
                "seed": r.seed,
                "facts": r.facts,
                "said": r.facts.is_none().then_some(&r.said),
            })
        })
        .collect();
    serde_json::json!({
        "scenario": request.scenario.display().to_string(),
        "program": program,
        "no_faults": request.no_faults,
        "from": request.seeds.0,
        "to": request.seeds.1,
        "seeds": ran.len(),
        "expect": request.expect,
        "at_least": request.at_least,
        "hits": hits,
        "other": other,
        "holds": holds,
        "found": found.len(),
        "clean": clean.len(),
        "incomplete": incomplete,
        "kinds": kinds,
        "results": results,
    })
}

/// The sweep as a person reads it.
fn print_sweep(request: &Sweep, ran: &[Ran]) {
    let clean: Vec<u64> = ran
        .iter()
        .filter(|r| r.facts.as_ref().is_some_and(|f| !f.found()))
        .map(|r| r.seed)
        .collect();
    let lost: Vec<u64> = ran.iter().filter(|r| r.facts.is_none()).map(|r| r.seed).collect();
    println!();
    match &request.expect {
        Some(code) => {
            let hits: Vec<u64> = ran
                .iter()
                .filter(|r| r.facts.as_ref().is_some_and(|f| f.names(code)))
                .map(|r| r.seed)
                .collect();
            // Counted, not subtracted: with `--expect Clean` a clean seed is also a hit.
            let other = ran
                .iter()
                .filter(|r| r.facts.as_ref().is_some_and(|f| f.found() && !f.names(code)))
                .count();
            println!(
                "  {} seeds · {} found {code} · {other} other findings · {} clean · {} did not complete",
                ran.len(),
                hits.len(),
                clean.len(),
                lost.len()
            );
            let missing: Vec<u64> =
                ran.iter().map(|r| r.seed).filter(|s| !hits.contains(s)).collect();
            if let Some(at_least) = request.at_least {
                let holds = hits.len() as u64 >= at_least;
                println!(
                    "  {}  at least {at_least} of {}",
                    if holds { "holds" } else { "FAILS" },
                    ran.len()
                );
            }
            println!("\n  first seeds with the code    {}", listed(&hits));
            println!("  seeds without it             {}", listed(&missing));
        }
        None => {
            let found: Vec<&Ran> =
                ran.iter().filter(|r| r.facts.as_ref().is_some_and(Facts::found)).collect();
            println!(
                "  {} seeds · {} findings · {} clean · {} did not complete",
                ran.len(),
                found.len(),
                clean.len(),
                lost.len()
            );
            let mut kinds: std::collections::BTreeMap<String, Vec<u64>> =
                std::collections::BTreeMap::new();
            for r in &found {
                if let Some(f) = &r.facts {
                    let key = f.codes.first().cloned().unwrap_or_else(|| f.outcome.clone());
                    kinds.entry(key).or_default().push(r.seed);
                }
            }
            if !kinds.is_empty() {
                println!();
            }
            for (kind, seeds) in &kinds {
                println!("  {kind:<22} {:>4}  seeds {}", seeds.len(), listed(seeds));
            }
        }
    }
    if let Some(first) = ran.iter().find(|r| r.facts.is_none()) {
        println!("\n  seed {} did not complete: {}", first.seed, first.said);
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(seed: u64, outcome: &str, code: Option<&str>) -> Ran {
        Ran {
            seed,
            facts: Some(Facts {
                outcome: outcome.into(),
                codes: code.map(|c| vec![c.to_owned()]).unwrap_or_default(),
                ..Facts::default()
            }),
            said: String::new(),
            wall: Duration::ZERO,
        }
    }

    #[test]
    fn finds_counts_the_code_and_holds_at_the_bound() {
        let swept = [
            ran(1, "ProbeViolated", Some("BOHR-2043")),
            ran(2, "Clean", None),
            ran(3, "ProbeViolated", Some("BOHR-2043")),
            ran(4, "GuestCrashed", None),
            Ran { seed: 5, facts: None, said: "killed".into(), wall: Duration::ZERO },
        ];
        let v = finds("finds", &swept, "BOHR-2043", 2);
        assert_eq!(v.holds, Some(true), "{}", v.line);
        assert!(v.line.starts_with("2 of 5 seeds found BOHR-2043"), "{}", v.line);
        assert!(v.line.contains("1 other findings · 1 did not complete"), "{}", v.line);
        assert_eq!(finds("finds", &swept, "BOHR-2043", 3).holds, Some(false));
        assert_eq!(finds("finds", &swept, "GuestCrashed", 1).holds, Some(true));
    }

    /// A seed that did not complete is not a clean seed: `never_finds` over a range where runs died
    /// would otherwise hold on a program nobody ran.
    #[test]
    fn never_finds_does_not_hold_over_runs_that_did_not_complete() {
        assert_eq!(
            never_finds("n", &[ran(1, "Clean", None), ran(2, "Clean", None)]).holds,
            Some(true)
        );
        assert_eq!(
            never_finds("n", &[ran(1, "Clean", None), ran(2, "GuestHung", None)]).holds,
            Some(false)
        );
        let lost = Ran { seed: 3, facts: None, said: "gone".into(), wall: Duration::ZERO };
        assert_eq!(never_finds("n", &[ran(1, "Clean", None), lost]).holds, Some(false));
    }

    /// A plan names each check with its label and scenario; a generated scenario is said to be
    /// generated rather than given a path nobody can open.
    #[test]
    fn a_plan_is_each_check_with_its_scenario() {
        let check = |name, generated: Option<&str>| Check {
            name,
            scenario: PathBuf::from("bug/s.toml"),
            generated: generated.map(str::to_owned),
            kind: Kind::Repeats { seed: 3, times: 4 },
        };
        let subject = Subject {
            name: "bug".to_owned(),
            about: "a seeded defect".to_owned(),
            cwd: PathBuf::from("."),
            pack: None,
            groups: Vec::new(),
        };
        let label = Some("lost-write".to_owned());
        let (given, generated) = (check("repeats", None), check("works", Some("[run]\n")));
        let plan = planned(&subject, &[(&None, &given), (&label, &generated)]);
        let value =
            crate::json::object("check", serde_json::json!({ "plan": true, "subjects": [plan] }));
        assert_eq!(value["schema"], "bohr.check/1");
        let checks = &value["subjects"][0]["checks"];
        assert_eq!(value["subjects"][0]["subject"], "bug");
        assert_eq!(checks[0]["name"], "repeats");
        assert_eq!(checks[0]["label"], serde_json::Value::Null);
        assert_eq!(checks[0]["generated"], false);
        assert!(checks[0]["scenario"].as_str().is_some_and(|s| s.ends_with("s.toml")));
        assert!(checks[0]["description"].as_str().is_some_and(|d| d.contains("seed 3 × 4")));
        assert_eq!(checks[1]["label"], "lost-write");
        assert_eq!(checks[1]["scenario"], serde_json::Value::Null);
    }

    /// A sweep's object carries every seed's facts and the count `--at-least` is held to.
    #[test]
    fn a_sweep_object_counts_against_its_bound() {
        let request = Sweep {
            scenario: PathBuf::from("s.toml"),
            seeds: (1, 4),
            expect: Some("BOHR-2043".to_owned()),
            at_least: Some(2),
            no_faults: false,
            jobs: 1,
            program: None,
        };
        let swept_runs = [
            ran(1, "ProbeViolated", Some("BOHR-2043")),
            ran(2, "Clean", None),
            ran(3, "GuestCrashed", None),
            Ran { seed: 4, facts: None, said: "killed".into(), wall: Duration::ZERO },
        ];
        let value = crate::json::object("sweep", swept(&request, "./node", &swept_runs));
        assert_eq!(value["schema"], "bohr.sweep/1");
        assert_eq!(value["seeds"], 4);
        assert_eq!(value["hits"], serde_json::json!([1]));
        assert_eq!(value["other"], 1);
        assert_eq!(value["holds"], false);
        assert_eq!(value["found"], 2);
        assert_eq!(value["clean"], 1);
        assert_eq!(value["incomplete"], serde_json::json!([4]));
        assert_eq!(value["kinds"]["BOHR-2043"], serde_json::json!([1]));
        assert_eq!(value["results"][0]["facts"]["outcome"], "ProbeViolated");
        assert_eq!(value["results"][3]["facts"], serde_json::Value::Null);
        assert_eq!(value["results"][3]["said"], "killed");
    }

    #[test]
    fn a_survey_table_reads_as_names_and_walls() {
        let text = "survey  seed 1  900 events  walls below\n\n   nr  name                    calls  wall\n  254  inotify_init1               1  answered ENOSYS\n  425  io_uring_setup              2  nothing models it\n\nthis is one path";
        assert_eq!(
            surveyed(text),
            [
                ("inotify_init1".to_owned(), "answered ENOSYS".to_owned()),
                ("io_uring_setup".to_owned(), "nothing models it".to_owned()),
            ]
        );
    }
}
