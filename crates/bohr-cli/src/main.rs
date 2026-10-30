//! The `bohr` binary.
//!
//! The only place where an implementation of each port is chosen and wired: a backend, a world
//! model, a scheduling policy and an oracle. Everything below is written against the traits.

#![forbid(unsafe_code)]

mod doctor;
mod execute;
mod explore;
mod guests;
mod handover;
mod reduce;
mod replay;
mod survey;
mod wire;

use std::process::ExitCode;

use bohr_kernel::RunConfig;
use bohr_tape::{Tape, TAPE_ABI};
use bohr_trace::{Manifest, Outcome, BUNDLE_FORMAT, SDK_SCHEMA};
use bohr_types::Seed;
use clap::{Parser, Subcommand};

use crate::guests::Demo;

/// Exit codes are part of the contract: a pipeline branches on them, so "the tool found a bug" and
/// "the tool is broken" must never share one. See `docs/API.md` §7.6.
mod exit {
    /// Nothing was found.
    pub(crate) const CLEAN: u8 = 0;
    /// Something was found. A success for the tool.
    pub(crate) const FINDING: u8 = 1;
    /// The invocation or the configuration is wrong.
    pub(crate) const USAGE: u8 = 2;
    /// A defect in the tool.
    pub(crate) const INTERNAL: u8 = 3;
    /// Determinism could not be guaranteed and the caller asked to be told so.
    ///
    /// Only a real backend can degrade, so a build without one never reaches this.
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
    pub(crate) const DEGRADED: u8 = 4;
}

/// What `bohr selftest` interleaves with. Uniform choice, because the guests it runs are checks on
/// the machinery rather than a search for bugs in them.
const SELFTEST_SCHEDULER: &str = "random";

/// Interleaving policies this build implements.
///
/// A policy named here but absent from the binary would silently run something else, so the list is
/// the set of policies that exist rather than the set that is planned.
const SCHEDULERS: [&str; 2] = ["pct", "random"];

/// Failures that stop a command before it produces a verdict.
#[derive(Debug)]
pub(crate) enum CliError {
    /// The invocation or configuration cannot be acted on.
    Usage(String),
    /// The tool reached a state it does not define behaviour for.
    Internal(String),
}

/// A configuration the world refused is a usage error here: the file is the user's, and what is
/// wrong with it is already said in the message the parser produced.
impl From<bohr_world::ConfigError> for CliError {
    fn from(error: bohr_world::ConfigError) -> Self {
        Self::Usage(error.to_string())
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(m) | Self::Internal(m) => write!(f, "{m}"),
        }
    }
}

impl CliError {
    const fn code(&self) -> u8 {
        match self {
            Self::Usage(_) => exit::USAGE,
            Self::Internal(_) => exit::INTERNAL,
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "bohr", version, about = "Deterministic simulation testing", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a program deterministically.
    Run {
        /// Seed the run is derived from.
        #[arg(long)]
        seed: Option<u64>,
        /// Use this tape instead of deriving one from a seed.
        #[arg(long, value_name = "FILE")]
        tape: Option<String>,
        /// Run this many times and require every run to be identical.
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Write a scenario bundle even for a clean run.
        #[arg(long, value_name = "FILE")]
        save: Option<String>,
        /// How execution is interleaved.
        #[arg(long, default_value = "pct")]
        scheduler: String,
        /// Exit with code 4 if the run could not be fully guaranteed.
        #[arg(long)]
        strict_determinism: bool,
        /// Abandon a run that records more than this many events.
        #[arg(long, value_name = "N")]
        max_events: Option<u64>,
        /// World configuration file: cluster, network, disk and scheduled faults.
        #[arg(long, value_name = "FILE")]
        config: Option<String>,
        /// Replace `RDRAND` and `RDSEED` in the prepared files with instructions that repeat.
        #[arg(long)]
        patch_rdrand: bool,
        /// Make the processor identification instruction fault, so this world answers it.
        #[arg(long)]
        patch_cpuid: bool,
        /// The program to run, after `--`. A `[differential]` scenario names its own.
        #[arg(last = true)]
        program: Vec<String>,
    },

    /// Replay a stored bundle and check it produces what it recorded.
    Replay {
        /// Where to find the recorded objects when their paths have moved.
        #[arg(long, value_name = "DIR")]
        binaries: Option<String>,
        /// Check the digest and say so; print nothing else.
        #[arg(long)]
        verify_only: bool,
        /// Stop before the first instruction and print a debugger attach line.
        #[arg(long)]
        gdb: bool,
        /// Stop the world at this event and hand it to a debugger.
        #[arg(long, value_name = "EVENT")]
        break_at: Option<u64>,
        /// The bundles to replay.
        #[arg(required = true)]
        bundle: Vec<String>,
    },

    /// Run the guests built into this binary and check they reproduce.
    ///
    /// Needs no program on disk and no privileges, so it answers "is this build deterministic on
    /// this machine" anywhere the binary runs.
    Selftest {
        /// Seed the runs are derived from.
        #[arg(long, default_value_t = 0)]
        seed: u64,
        /// How many times to repeat each guest.
        #[arg(long, default_value_t = 100)]
        repeat: usize,
        /// Run only this guest.
        #[arg(long, value_enum)]
        guest: Option<Demo>,
        /// Write a scenario bundle per guest into this directory.
        #[arg(long, value_name = "DIR")]
        save_dir: Option<String>,
    },

    /// Search for a finding, rather than run what was asked for.
    ///
    /// A campaign is a function of its master seed and its run budget: `--runs` reproduces
    /// exactly, whatever `-j` says, because the work is planned a generation at a time and folded
    /// in the order it was planned. `--time` is the convenient budget and records how many runs it
    /// reached, so the same campaign can be repeated with `--runs`. See `docs/API.md` §7.2.
    Explore {
        /// Wall-clock budget, in seconds. Convenient, and not reproducible on its own.
        #[arg(long, value_name = "SECONDS")]
        time: Option<u64>,
        /// Run budget. The reproducible one.
        #[arg(long, value_name = "N")]
        runs: Option<u64>,
        /// How many workers.
        #[arg(long, short = 'j', default_value_t = 1)]
        jobs: usize,
        /// Where findings are written.
        #[arg(long, value_name = "DIR", default_value = "findings")]
        findings: String,
        /// A corpus to carry in and write back out.
        #[arg(long, value_name = "DIR")]
        corpus: Option<String>,
        /// World configuration: cluster, network, disk and drawn faults.
        #[arg(long, value_name = "FILE")]
        config: Option<String>,
        /// Reproduces the whole campaign.
        #[arg(long, value_name = "U64", default_value_t = 0)]
        master_seed: u64,
        /// Write a machine-readable account of the campaign here.
        #[arg(long, value_name = "FILE")]
        summary: Option<String>,
        /// Search a guest built into this binary instead of a program on disk.
        #[arg(long, value_enum)]
        guest: Option<Demo>,
        /// The program to search, after `--`.
        #[arg(last = true)]
        program: Vec<String>,
    },

    /// Carry out one slice of a campaign. Started by `bohr explore`, not by a person.
    #[command(hide = true)]
    ExploreWorker {
        /// The work to do.
        #[arg(long, value_name = "FILE")]
        work: String,
        /// Where to write what happened.
        #[arg(long, value_name = "FILE")]
        results: String,
        /// World configuration, the same file the campaign was given.
        #[arg(long, value_name = "FILE")]
        config: Option<String>,
        /// The guest built into this binary to run.
        #[arg(long, value_enum)]
        guest: Option<Demo>,
        /// The program to run, after `--`.
        #[arg(last = true)]
        program: Vec<String>,
    },

    /// List the syscalls a program asks for that this build does not model.
    ///
    /// Not a run: every unmodelled call is answered `ENOSYS` so the next one can be reached, so
    /// nothing here says whether the program behaves. It writes no bundle and exits 4 when it
    /// found anything, because what it found is about this build rather than about the guest.
    Survey {
        /// Seed the run is derived from.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// World configuration file, where the program is a cluster.
        #[arg(long, value_name = "FILE")]
        config: Option<String>,
        /// Abandon the survey after this many events.
        #[arg(long, value_name = "N")]
        max_events: Option<u64>,
        /// The program to survey, after `--`.
        #[arg(last = true)]
        program: Vec<String>,
    },

    /// Report what this host can and cannot do, and what to type about it.
    Doctor {
        /// Also inspect a program this host would be asked to run.
        #[arg(long, value_name = "PROGRAM")]
        target: Option<String>,
        /// Print the syscall support table instead of checking anything.
        #[arg(long)]
        syscalls: bool,
    },

    /// Reduce a stored finding to the smallest scenario that still reproduces it.
    Reduce {
        /// The bundle to reduce.
        bundle: String,
        /// Where the reduced bundle is written.
        #[arg(short = 'o', long, value_name = "PATH")]
        out: String,
        /// Most verification replays to spend.
        #[arg(long, default_value_t = 2000)]
        budget: u32,
        /// What to minimise.
        #[arg(long, value_name = "WHAT", default_value = "both")]
        keep: String,
        /// Where to find the recorded objects if their paths moved.
        #[arg(long)]
        binaries: Option<String>,
    },

    /// Write a stored run as one page a person can read.
    Report {
        /// The bundle to render.
        bundle: String,
        /// Where the page is written.
        #[arg(short = 'o', long, value_name = "PATH", default_value = "report.html")]
        out: String,
        /// Also write a bundle without what a customer would not want published.
        #[arg(long)]
        redact: bool,
    },

    /// Write a line on a stored run, for whoever opens it next.
    Note {
        /// The bundle to write on.
        bundle: String,
        /// What to say.
        text: String,
    },

    /// Print a stored run's event log.
    ///
    /// The whole log, in the order it was recorded, without replaying anything: what a bundle
    /// carries is what this prints. `json` is the same projection a report leaves beside itself
    /// for the events it did not show.
    Trace {
        /// The bundle to read.
        bundle: String,
        /// How to write it.
        #[arg(long, value_name = "FORMAT", default_value = "text")]
        format: String,
    },

    /// Print the version, and the four contract numbers a bug report needs.
    Version {
        /// Print every contract number rather than the tool version alone.
        #[arg(long)]
        verbose: bool,
    },
}

/// The world a campaign runs in, read once and carried with the text it came from.
///
/// The text travels because a finding's bundle records the configuration that produced it, and a
/// bundle that named a file rather than carrying its contents would replay differently the moment
/// somebody edited it.
fn read_scenario(
    path: Option<&str>,
) -> Result<Option<(bohr_world::LoadedConfig, String)>, CliError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    Ok(Some((bohr_world::config::load(&text)?, text)))
}

/// What a campaign was pointed at: a guest built in, or a program on disk, and never both.
fn subject_of(guest: Option<Demo>, program: Vec<String>) -> Result<explore::Subject, CliError> {
    match (guest, program.is_empty()) {
        (Some(demo), true) => Ok(explore::Subject::Demo(demo)),
        (None, false) => Ok(explore::Subject::Program(program)),
        (Some(_), false) => Err(CliError::Usage(
            "BOHR-1001: name a built-in guest or a program, not both".to_owned(),
        )),
        (None, true) => Err(CliError::Usage(
            "BOHR-1001: nothing to search; name a program after `--` or a guest with --guest"
                .to_owned(),
        )),
    }
}

/// The components a run is wired with, by whether it is configured as a cluster.
///
/// Named in one place because a bundle records them and a replay checks them: two answers to
/// "which world was this" is how a bundle comes to be refused for the wrong reason.
pub(crate) fn identities(
    loaded: Option<&bohr_world::LoadedConfig>,
) -> (bohr_types::WorldModelId, bohr_types::OracleId) {
    let world = if loaded.is_some() {
        bohr_kernel::WorldModel::identity(&bohr_world::DistributedWorld::new())
    } else {
        bohr_kernel::WorldModel::identity(&bohr_world::MinimalWorld::new())
    };
    // The same panel the run would be judged by, asked for its name rather than told one: two
    // places that each spelled the identity out would be two places that can disagree, and the
    // disagreement is only visible when a replay refuses a bundle that was fine.
    (
        world,
        bohr_kernel::Oracle::identity(&bohr_world::panel(
            loaded.map(bohr_world::LoadedConfig::watching).unwrap_or_default(),
        )),
    )
}

/// The bounds a run is carried out under, from the scenario and the command line.
///
/// One place, because a horizon that reached some commands and not others would be worse than one
/// that reached none: a bundle recorded with a horizon and replayed without it diverges at the
/// first event past where the horizon would have ended it, and the divergence is reported as a
/// defect in the tool rather than as the missing bound it is.
pub(crate) fn run_config_for(
    loaded: Option<&bohr_world::LoadedConfig>,
    max_events: Option<u64>,
) -> RunConfig {
    let mut config = RunConfig::default();
    if let Some(limits) = loaded.map(|l| l.run) {
        config.horizon = limits.horizon;
        config.ends_with = limits.ends_with_node.map(bohr_types::NodeId);
        if let Some(budget) = limits.max_ticks {
            config.max_ticks = budget;
        }
    }
    if let Some(budget) = max_events {
        config.max_events = budget;
    }
    config
}

/// One guest specification per node, from a command line and the configuration.
///
/// A single-node `[cluster]` may still shape the run: its environment is presented, and `{node}` in
/// its argv is substituted, so a scenario file and the command line agree on what node zero is.
/// Without a `[cluster]`, the command line is node zero and there is nothing else.
///
/// **`standing_in` is one execution of a comparison, and it replaces exactly one node.** A cluster
/// of unlike nodes is how a real client is driven at all — one node is the client, another is the
/// caller that makes it do anything — so replacing the whole cluster would replace the caller too,
/// and every implementation would then run the identical scenario and agree with itself. That was
/// the behaviour here until this parameter existed: a cluster's own programs won, silently, and a
/// comparison of two clients reported agreement having compared one.
///
/// **One function, and it matters.** A run and its replay build their guests from different
/// starting points — a command line here, a stored configuration there — and for a while they did
/// so through different code. The environment they produced differed by two variables, which moved
/// the guest's stack by sixty-four bytes, and any guest that read a stack address replayed to a
/// different digest than it recorded. A divergence in the one thing the tool exists to guarantee,
/// caused by nothing more than two spellings of one rule.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
/// Writes a copy of a program with its processor questions made to fault, and names the copy.
///
/// A copy and never the file itself: rewriting what somebody handed us is the one thing
/// `ARCHITECTURE.md` §5.8 forbids outright. The copy lands beside the original with a suffix, so a
/// reader looking at a report and a reader looking at a directory see the same name.
fn trapped_copy(program: &str) -> Result<String, CliError> {
    let bytes = std::fs::read(program)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {program}: {e}")))?;
    let Some((patched, sites)) = bohr_backend::code::cpuid::trapped(&bytes) else {
        eprintln!("no processor questions to trap in {program}");
        return Ok(program.to_owned());
    };
    let into = std::env::temp_dir().join(format!(
        "{}.bohr-trapped",
        std::path::Path::new(program).file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&into, &patched)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {}: {e}", into.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&into, std::fs::Permissions::from_mode(0o755));
    }
    eprintln!("trapped {} processor question(s) in {program}", sites.len());
    Ok(into.to_string_lossy().into_owned())
}

pub(crate) fn specs_for(
    argv: Vec<String>,
    loaded: Option<&bohr_world::LoadedConfig>,
    standing_in: Option<&bohr_world::Implementation>,
) -> Vec<bohr_backend::GuestSpec> {
    // The one node a comparison replaces, where this is one execution of one. Everything else the
    // scenario describes is left alone: the caller that drives the node under comparison is part of
    // the conditions, not part of what is being compared.
    let standing_in = standing_in.and_then(|i| i.argv_for().map(|own| (i.node, own)));

    match loaded.and_then(|l| l.cluster.as_ref()) {
        Some(cluster) => (0..cluster.nodes.max(1))
            .map(|index| {
                let argv = match &standing_in {
                    Some((node, own)) if *node == index => own.clone(),
                    _ if cluster.argv.is_empty() && cluster.programs.is_empty() => argv.clone(),
                    _ => cluster.argv_for(index),
                };
                let mut env = cluster.env.clone();
                // Where the guest's SDK writes what it has to say about itself, and which node
                // it is. Both are presented rather than inherited: a guest that read them from
                // the host would be told about a machine it is not on.
                env.push(("BOHR_SDK_OUTPUT".to_owned(), execute::SDK_RECORD_PATH.to_owned()));
                env.push(("BOHR_SDK_NODE".to_owned(), index.to_string()));
                bohr_backend::GuestSpec { argv, env }
            })
            .collect(),
        None => {
            let argv = standing_in.map_or(argv, |(_, own)| own);
            vec![bohr_backend::GuestSpec { argv, env: Vec::new() }]
        }
    }
}

/// Runs every implementation a differential scenario names, and says what came of it.
///
/// **`Skipped` is not clean, and that is the sharpest thing here.** A group verdict of "nothing was
/// compared" — a projection path nobody writes, a label one implementation never produced — folded
/// into a clean run would let a campaign of a million differential runs report a million clean runs
/// while comparing nothing at all. It gets a code of its own and a non-zero exit.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn compare_implementations(
    program: Vec<String>,
    tape: &Tape,
    config: RunConfig,
    scheduler: &str,
    scenario: &bohr_world::LoadedConfig,
    config_toml: &str,
) -> Result<u8, CliError> {
    let group = execute::compare(program, tape, config, scheduler, scenario, config_toml)?;

    for (name, completed) in group.names.iter().zip(&group.executions) {
        eprintln!(
            "{name:<20} {:<8} {} events · {}",
            bohr_report::describe::outcome_word(&completed.report.outcome),
            completed.report.events,
            completed.report.digest
        );
    }

    // An execution that went wrong on its own is a sharper answer than "they disagreed", and it is
    // also why they would have: an implementation that crashed produced no value to compare.
    for (name, completed) in group.names.iter().zip(&group.executions) {
        if !matches!(completed.report.outcome, Outcome::Clean) {
            eprintln!("{name}: {:?}", completed.report.outcome);
            return Ok(exit::FINDING);
        }
    }

    match group.verdict {
        bohr_kernel::Verdict::Agree => {
            println!("implementations agreed on every projection");
            Ok(exit::CLEAN)
        }
        bohr_kernel::Verdict::Finding(outcome) => {
            println!("{}", bohr_report::describe::outcome_word(&outcome));
            eprintln!("{outcome:?}");
            Ok(exit::FINDING)
        }
        bohr_kernel::Verdict::Clean | bohr_kernel::Verdict::Skipped => Err(CliError::Internal(
            "BOHR-5004: a differential run compared nothing. Every implementation has to produce \
             every projection, and one that produced none leaves a run that looks clean and checked \
             nothing."
                .to_owned(),
        )),
    }
}

fn main() -> ExitCode {
    match dispatch() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(e.code())
        }
    }
}

fn dispatch() -> Result<u8, CliError> {
    match Cli::parse().command {
        Command::Version { verbose } => {
            version(verbose);
            Ok(exit::CLEAN)
        }
        Command::Run {
            seed,
            tape,
            repeat,
            save,
            scheduler,
            strict_determinism,
            max_events,
            config: config_path,
            patch_rdrand,
            patch_cpuid,
            program,
        } => {
            if !SCHEDULERS.contains(&scheduler.as_str()) {
                return Err(CliError::Usage(format!(
                    "BOHR-1002: no scheduler named {scheduler}; this build has {}",
                    SCHEDULERS.join(", ")
                )));
            }
            let tape = load_tape(seed, tape.as_deref())?;
            // The text is kept as well as the parsed form: a bundle records the configuration
            // verbatim, and re-serialising a parsed one would record what we understood of it.
            let config_text =
                match config_path.as_deref() {
                    Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
                        CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}"))
                    })?),
                    None => None,
                };
            let mut loaded = match config_text.as_deref() {
                Some(text) => Some(bohr_world::config::load(text)?),
                None => None,
            };
            // Said out loud, once, before the run: a guest that asked the processor for a number
            // and was handed a fixed one is not the guest anybody shipped, and a reader looking at
            // the report later has to be able to see that this was done.
            if patch_rdrand {
                let state = loaded.as_mut().and_then(|scenario| scenario.state.take());
                match state {
                    Some(image) => {
                        let (patched, rewritten) = image.without_hardware_randomness();
                        for (path, sites) in &rewritten {
                            eprintln!("patched {sites} hardware randomness site(s) in {path}");
                        }
                        if rewritten.is_empty() {
                            eprintln!("no hardware randomness to patch in the prepared files");
                        }
                        if let Some(scenario) = loaded.as_mut() {
                            scenario.state = Some(patched);
                        }
                    }
                    None => {
                        return Err(CliError::Usage(
                            "BOHR-1001: --patch-rdrand rewrites the prepared files, and this                              scenario prepares none; see [state] in docs/API.md §3"
                                .to_owned(),
                        ))
                    }
                }
            }
            // The program is not among the prepared files -- it is started by the interceptor and
            // mapped by the kernel -- so it is rewritten into a copy of its own and the run is
            // pointed at that. Its own path is what a bundle records and what `/proc/self/exe`
            // answers, so the copy is what the run was of, and saying so is the point of printing
            // it.
            let mut program = program;
            if patch_cpuid {
                if let Some(state) = loaded.as_mut().and_then(|scenario| scenario.state.take()) {
                    let (patched, rewritten) = state.with_processor_questions_trapped();
                    for (path, sites) in &rewritten {
                        eprintln!("trapped {sites} processor question(s) in {path}");
                    }
                    if let Some(scenario) = loaded.as_mut() {
                        scenario.state = Some(patched);
                    }
                }
                if let Some(first) = program.first().cloned() {
                    let copy = trapped_copy(&first)?;
                    if let Some(slot) = program.first_mut() {
                        *slot = copy;
                    }
                }
            }
            let config = run_config_for(loaded.as_ref(), max_events);
            run_program(
                program,
                &tape,
                repeat,
                save.as_deref(),
                strict_determinism,
                &scheduler,
                config,
                loaded.as_ref(),
                config_text.as_deref().unwrap_or(""),
            )
        }
        Command::Replay { binaries, verify_only, gdb, break_at, bundle } => {
            // Refused here rather than after replaying: a run that ends in a hand-over nobody can
            // use has cost as long as the original took to learn nothing.
            let break_at =
                match (gdb, break_at) {
                    (true, Some(_)) => return Err(CliError::Usage(
                        "BOHR-1001: --gdb is --break-at 0; naming both says two different things"
                            .to_owned(),
                    )),
                    (true, None) => Some(0),
                    (false, at) => at,
                };
            if break_at.is_some() {
                if verify_only {
                    return Err(CliError::Usage(
                        "BOHR-1001: --verify-only checks a digest, and a hand-over never reaches \
                         one"
                        .to_owned(),
                    ));
                }
                if bundle.len() > 1 {
                    return Err(CliError::Usage(
                        "BOHR-1001: a hand-over is one frozen world; name one bundle".to_owned(),
                    ));
                }
            }
            Ok(replay_bundles(&bundle, binaries.as_deref(), verify_only, break_at))
        }
        Command::Explore {
            time,
            runs,
            jobs,
            findings,
            corpus,
            config,
            master_seed,
            summary,
            guest,
            program,
        } => {
            let subject = subject_of(guest, program)?;
            let scenario = read_scenario(config.as_deref())?;
            if time.is_none() && runs.is_none() {
                return Err(CliError::Usage(
                    "BOHR-1001: a campaign needs a budget: --runs for one that reproduces,                      --time for one that does not"
                        .to_owned(),
                ));
            }
            explore::explore(&explore::Request {
                master: Seed(master_seed),
                runs,
                time: time.map(std::time::Duration::from_secs),
                jobs,
                findings: findings.into(),
                corpus: corpus.map(Into::into),
                summary: summary.map(Into::into),
                scenario_path: config.map(Into::into),
                scenario,
                subject,
            })
        }
        Command::ExploreWorker { work, results, config, guest, program } => {
            let scenario = read_scenario(config.as_deref())?;
            explore::work(
                &work,
                &results,
                &explore::Request {
                    master: Seed(0),
                    runs: None,
                    time: None,
                    jobs: 1,
                    findings: std::path::PathBuf::new(),
                    corpus: None,
                    summary: None,
                    scenario_path: config.map(Into::into),
                    scenario,
                    subject: subject_of(guest, program)?,
                },
            )
        }
        Command::Reduce { bundle, out, budget, keep, binaries } => {
            reduce::reduce(&bundle, &out, budget, &keep, binaries.as_deref())
        }
        Command::Report { bundle, out, redact } => report(&bundle, &out, redact),
        Command::Survey { seed, config, max_events, program } => {
            if program.is_empty() {
                return Err(CliError::Usage(
                    "BOHR-1002: a survey needs a program, after `--`".to_owned(),
                ));
            }
            survey::run(program, seed, config.as_deref(), max_events)
        }
        Command::Note { bundle, text } => note(&bundle, &text),
        Command::Trace { bundle, format } => trace(&bundle, &format),
        Command::Doctor { target, syscalls } => doctor::run(target.as_deref(), syscalls),
        Command::Selftest { seed, repeat, guest, save_dir } => {
            selftest(Seed(seed), repeat, guest, save_dir.as_deref())
        }
    }
}

/// The tape a run is driven by: a stored one, or one derived from a seed.
///
/// A stored tape already carries the seed it came from, so naming both leaves two answers to one
/// question and is refused rather than resolved by precedence.
fn load_tape(seed: Option<u64>, file: Option<&str>) -> Result<Tape, CliError> {
    let Some(path) = file else {
        return Ok(Tape::from_seed(Seed(seed.unwrap_or(0))));
    };
    if seed.is_some() {
        return Err(CliError::Usage(
            "BOHR-1004: --seed and --tape name different runs; give one".to_owned(),
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    Tape::from_container(&bytes).map_err(|e| CliError::Usage(e.to_string()))
}

/// Replays every named bundle, reporting the worst outcome any of them reached.
///
/// The worst rather than the first: a glob of bundles is checked to learn whether they all still
/// replay, and stopping at the first failure would hide how many others are broken too.
fn replay_bundles(
    paths: &[String],
    binaries: Option<&str>,
    verify_only: bool,
    break_at: Option<u64>,
) -> u8 {
    let mut worst = exit::CLEAN;
    for path in paths {
        // A bundle that cannot be read is reported like one that failed to replay, so a glob
        // with a hole in it still says what happened to every other bundle in it.
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("{path}: BOHR-1002: cannot read it: {e}");
                worst = worst.max(exit::USAGE);
                continue;
            }
        };
        match replay::replay_to(&bytes, binaries, break_at) {
            Ok(replay::Verdict::Matched(manifest)) => {
                if verify_only {
                    println!("{path}: ok {}", manifest.trace_digest);
                } else {
                    report_replay(path, &manifest);
                }
            }
            Ok(replay::Verdict::HandedOver) => {}
            Ok(replay::Verdict::Diverged(detail)) => {
                eprintln!("{path}: DIVERGED");
                eprintln!("{detail}");
                worst = worst.max(exit::INTERNAL);
            }
            Err(e) => {
                eprintln!("{path}: {e}");
                worst = worst.max(e.code());
            }
        }
    }
    worst
}

/// Writes a stored run as one page, and the events it could not draw beside it.
///
/// Two files rather than one only where the run was too long to draw whole. The page names the
/// second and says what is in it, so a reader who has both knows why, and a reader who has only
/// the page is not left wondering what it is not showing them.
fn report(path: &str, out: &str, redact: bool) -> Result<u8, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let bundle = bohr_trace::Bundle::parse(&bytes).map_err(|e| CliError::Usage(e.to_string()))?;

    // One suffix and not every one of them: `trim_end_matches` strips repeats, so `a.html.html`
    // became `a` and the sidecar took a name the reader would not have guessed.
    let stem = out.strip_suffix(".html").unwrap_or(out);
    let sidecar_name = format!("{stem}.events.jsonl");
    let short = std::path::Path::new(&sidecar_name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| sidecar_name.clone());
    // The page prints a command to copy, so it is told what the bundle it was made from is
    // called rather than deriving a name from the run identifier that nothing writes.
    let named = std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned());
    let rendered = bohr_report::render_from(&bundle, Some(&short), Some(&named))
        .map_err(|e| CliError::Usage(e.to_string()))?;

    if rendered.html.len() > bohr_report::MOST_BYTES {
        eprintln!(
            "the page is {} bytes, past the {} a report is meant to stay under; written anyway, \
             and \n worth looking at rather than attaching",
            rendered.html.len(),
            bohr_report::MOST_BYTES
        );
    }
    std::fs::write(out, &rendered.html)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {out}: {e}")))?;
    println!("{out}");

    if let Some(events) = &rendered.sidecar {
        std::fs::write(&sidecar_name, events)
            .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {sidecar_name}: {e}")))?;
        println!("{sidecar_name}");
    }

    if redact {
        // Beside the original rather than over it: what circulates is a pair, and the bundle with
        // everything in it is what the person who ran it keeps.
        let cleaned = bohr_trace::redact(&bundle).map_err(|e| CliError::Usage(e.to_string()))?;
        let name = format!("{}.redacted.bohr", path.trim_end_matches(".bohr"));
        std::fs::write(&name, cleaned)
            .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {name}: {e}")))?;
        println!("{name}");
    }
    Ok(exit::CLEAN)
}

/// Adds a line to a bundle's notes, in place.
///
/// No timestamp and no author. The host clock is banned everywhere a run could read it, and a note
/// that carried one would make two identical invocations produce different bytes — which is the
/// property every stored artefact in this tool is built to have. What a note is for is the sentence
/// somebody wants the next reader to see.
fn note(path: &str, text: &str) -> Result<u8, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let bundle = bohr_trace::Bundle::parse(&bytes).map_err(|e| CliError::Usage(e.to_string()))?;

    let mut notes = bundle
        .section(bohr_trace::SectionId::Notes)
        .map(|note| String::from_utf8_lossy(note).into_owned())
        .unwrap_or_default();
    if !notes.is_empty() && !notes.ends_with('\n') {
        notes.push('\n');
    }
    notes.push_str(text.trim());
    notes.push('\n');

    let mut writer = bundle.to_writer();
    writer.section(bohr_trace::SectionId::Notes, notes.clone().into_bytes());
    // **Written beside and moved into place, never over the top.** `write` truncates first, so a
    // crash or a full disk part-way through left nothing where a finding had been -- and a finding
    // is the one file in this tool that cannot be produced again on demand. A rename is atomic on
    // every filesystem this runs on, so the bundle is either the old one or the new one.
    let staged = format!("{path}.writing");
    std::fs::write(&staged, writer.finish(bundle.run_id))
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {staged}: {e}")))?;
    std::fs::rename(&staged, path).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        CliError::Usage(format!("BOHR-1002: cannot replace {path}: {e}"))
    })?;
    print!("{notes}");
    Ok(exit::CLEAN)
}

/// Prints a stored run's event log, in the form asked for.
///
/// Read rather than replayed: a bundle carries its log, and re-running the guest to print what is
/// already in the file would need the guest to still exist and would take as long as the run did.
fn trace(path: &str, format: &str) -> Result<u8, CliError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let bundle = bohr_trace::Bundle::parse(&bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    let model = bohr_report::RunModel::of(&bundle).map_err(|e| CliError::Usage(e.to_string()))?;

    let written = match format {
        "text" => bohr_report::text::write_log(&model.events),
        "json" => bohr_report::jsonl::write_log(&model.events),
        other => {
            return Err(CliError::Usage(format!(
                "BOHR-1001: no format named {other:?}; this build writes text and json"
            )))
        }
    };
    print!("{written}");

    // Said on stderr so it cannot land in a file somebody is parsing. A log that stops early is
    // the most misleading thing a bundle can hand a reader, because every event in it is true.
    if let Some(damage) = &model.truncated {
        eprintln!("{path}: the log ends early: {damage}");
    }
    Ok(exit::CLEAN)
}

/// What a matched replay says about the run it reproduced.
fn report_replay(path: &str, manifest: &Manifest) {
    println!("{path}: ok {}", manifest.trace_digest);
    print!("{}", bohr_report::summary(manifest));
}

fn version(verbose: bool) {
    let tool = env!("CARGO_PKG_VERSION");
    if !verbose {
        println!("bohr {tool}");
        return;
    }
    // All four appear in every bundle, and a bug report is unactionable without them.
    println!("bohr {tool}");
    println!("  tape ABI       {TAPE_ABI}");
    println!("  bundle format  {BUNDLE_FORMAT}");
    println!("  SDK schema     {SDK_SCHEMA}");
}

/// Runs a program from disk under interception.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[allow(clippy::too_many_arguments)]
fn run_program(
    program: Vec<String>,
    tape: &Tape,
    repeat: usize,
    save: Option<&str>,
    strict: bool,
    scheduler: &str,
    config: RunConfig,
    loaded: Option<&bohr_world::LoadedConfig>,
    config_toml: &str,
) -> Result<u8, CliError> {
    if repeat == 0 {
        return Err(CliError::Usage("BOHR-1002: --repeat must be at least 1".to_owned()));
    }
    // A differential scenario names its own programs, so the command line has nothing to add and
    // a program after `--` would be a third thing nobody asked to run.
    if let Some(scenario) = loaded.filter(|l| l.differential.is_some()) {
        // Refused rather than ignored. Both were accepted and silently did nothing here, so
        // somebody who asked for a bundle got a clean exit and no file, and learned it later.
        if save.is_some() {
            return Err(CliError::Usage(
                "BOHR-1002: --save has nothing to write for a differential run, which produces a                  group of executions rather than one"
                    .to_owned(),
            ));
        }
        if repeat != 1 {
            return Err(CliError::Usage(
                "BOHR-1002: --repeat is not carried out for a differential run; the comparison                  between implementations is what such a run repeats"
                    .to_owned(),
            ));
        }
        return compare_implementations(program, tape, config, scheduler, scenario, config_toml);
    }

    let path = program.first().cloned().unwrap_or_default();
    if path.is_empty() {
        return Err(CliError::Usage(
            "BOHR-1001: a run needs a program after `--`, or a scenario that names its own"
                .to_owned(),
        ));
    }
    if !std::path::Path::new(&path).is_file() {
        return Err(CliError::Usage(format!("BOHR-1001: {path} is not a file")));
    }

    let target = execute::Target::Program(specs_for(program, loaded, None));
    let exec = execute::Execution {
        target: &target,
        tape,
        config,
        output: execute::Output::Deliver,
        policy: scheduler,
        record: execute::Record::Log,
        scenario: loaded,
        config_toml,
    };

    match execute::repeat(exec, repeat)? {
        Err(divergence) => {
            eprintln!("{path}: DIVERGED");
            eprintln!("{divergence}");
            Ok(exit::FINDING)
        }
        Ok(completed) => {
            let armed = loaded.is_some_and(bohr_world::LoadedConfig::asks_for_adversity);
            report_run(&path, repeat, &completed, armed);
            if let Some(file) = save {
                let bytes = execute::to_bundle(&completed, tape, None)?;
                std::fs::write(file, bytes)
                    .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {file}: {e}")))?;
                eprintln!("  saved {file}");
            }
            if !matches!(completed.report.outcome, Outcome::Clean) {
                return Ok(exit::FINDING);
            }
            // A degraded run is reported either way; the exit code changes only where the caller
            // asked for the guarantee rather than for the result.
            Ok(if strict && !completed.degradations.is_empty() {
                exit::DEGRADED
            } else {
                exit::CLEAN
            })
        }
    }
}

/// Refuses a program where no interceptor exists to run it under.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[allow(clippy::too_many_arguments)]
fn run_program(
    program: Vec<String>,
    _tape: &Tape,
    _repeat: usize,
    _save: Option<&str>,
    _strict: bool,
    _scheduler: &str,
    _config: RunConfig,
    _loaded: Option<&bohr_world::LoadedConfig>,
    _config_toml: &str,
) -> Result<u8, CliError> {
    Err(CliError::Usage(format!(
        "BOHR-1001: cannot run {}: this build has no syscall backend. \
         `bohr selftest` exercises the guests built into the binary.",
        program.first().map_or("<program>", String::as_str)
    )))
}

/// One line per completed run, in the form both commands report.
///
/// Written to standard error, because standard output carries whatever the guest itself wrote and
/// mixing the two would corrupt both.
///
/// `armed` says the scenario asked for something to go wrong, which is read against what landed:
/// the two come apart, and a run that met no adversity at all reads exactly like one that met
/// plenty and answered it.
fn report_run(label: &str, repeat: usize, completed: &execute::Completed, armed: bool) {
    let outcome = &completed.report.outcome;
    let status = if matches!(outcome, Outcome::Clean) { "ok" } else { "finding" };
    let (syscalls, busiest) = execute::syscall_profile(&completed.log, 5);
    eprintln!(
        "{label:<18} {status:<8} {repeat} runs · {} events · {syscalls} syscalls · {} draws · {}",
        completed.report.events,
        completed.report.journal.len(),
        completed.report.digest
    );
    if !matches!(outcome, Outcome::Clean) {
        eprintln!("  {outcome:?}");
        for note in execute::last_diagnostics(&completed.log, 3) {
            eprintln!("  {note}");
        }
        // A run that hung says nothing about where. The calls it made most is the shortest route
        // to the loop it is in.
        if matches!(outcome, Outcome::GuestHung(_)) {
            for (nr, count) in busiest {
                let name = bohr_report::syscall_name(nr).unwrap_or("?");
                eprintln!("  {count:>9} × {nr:<4} {name}");
            }
        }
    }
    // Where the guest carries the coverage runtime, what it covered and whether the map could
    // hold it. A map with fewer counters than the guest has edges puts two edges on one, and the
    // search that follows cannot tell those two paths apart — see `docs/API.md` §5.5.
    if let (Some(map), edges @ 1..) = (&completed.coverage, completed.coverage_edges) {
        eprintln!("  covered {} of {edges} edges · map holds {}", map.edges(), map.cells());
        if edges > map.cells() as u64 {
            eprintln!("  the map is smaller than the guest: raise [coverage] shm_size to {edges}");
        }
    }
    if armed {
        eprintln!("  adversity landed {}", completed.report.adversity);
        if completed.report.adversity == 0 {
            eprintln!(
                "  nothing this scenario armed reached the guest: a moment written down or armed \
                 after the run ends fires into nothing"
            );
        }
    }
    if !completed.degradations.is_empty() {
        eprintln!("  degraded: {}", completed.degradations.join(", "));
    }
}

fn selftest(
    seed: Seed,
    repeat: usize,
    only: Option<Demo>,
    save_dir: Option<&str>,
) -> Result<u8, CliError> {
    if repeat == 0 {
        return Err(CliError::Usage("BOHR-1002: --repeat must be at least 1".to_owned()));
    }
    if let Some(dir) = save_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot use {dir}: {e}")))?;
    }

    let config = RunConfig::default();
    let guests: Vec<Demo> = only.map_or_else(|| Demo::all().to_vec(), |g| vec![g]);
    let mut worst = exit::CLEAN;

    let tape = Tape::from_seed(seed);
    for guest in guests {
        let target = execute::Target::Demo(guest.name().to_owned(), guest.program());
        let exec = execute::Execution {
            target: &target,
            tape: &tape,
            config,
            output: execute::Output::Deliver,
            policy: SELFTEST_SCHEDULER,
            record: execute::Record::Log,
            scenario: None,
            config_toml: "",
        };
        match execute::repeat(exec, repeat)? {
            Err(divergence) => {
                eprintln!("{:<18} DIVERGED", guest.name());
                eprintln!("{divergence}");
                worst = worst.max(exit::FINDING);
            }
            Ok(completed) => {
                // A built-in guest runs against no scenario, so nothing was armed to land.
                report_run(guest.name(), repeat, &completed, false);
                if !matches!(completed.report.outcome, Outcome::Clean) {
                    worst = worst.max(exit::FINDING);
                }
                if let Some(dir) = save_dir {
                    let path = format!("{dir}/{}.bohr", guest.name());
                    let bytes = execute::to_bundle(&completed, &tape, None)?;
                    std::fs::write(&path, bytes).map_err(|e| {
                        CliError::Usage(format!("BOHR-1002: cannot write {path}: {e}"))
                    })?;
                    eprintln!("  saved {path}");
                }
            }
        }
    }

    Ok(worst)
}
