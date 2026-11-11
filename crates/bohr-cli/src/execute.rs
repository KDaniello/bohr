//! Running a guest, and comparing runs against each other.

use bohr_backend::{CoverageMap, DeterminismBackend, GuestSpec};
use bohr_backend_null::{NullBackend, NullProgram};
use bohr_kernel::{
    FairPolicy, PctPolicy, RandomPolicy, RunConfig, RunInputs, RunReport, SchedulePolicy, run,
};
use bohr_tape::Tape;
use bohr_trace::{
    BinaryRecord, BundleWriter, Counters, Event, HostProfile, PolicyRecord, Provenance, SectionId,
    TraceWriter, derive_run_id, manifest_for, read_events,
};
use bohr_world::LoadedConfig;

use crate::CliError;

/// What to run.
pub(crate) enum Target {
    /// A guest built into the binary, executed in process, and the name it answers to.
    ///
    /// The name identifies the guest in a bundle, since a built-in guest has no path.
    Demo(String, NullProgram),
    /// Programs on disk — one per node — executed under interception.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Program(Vec<GuestSpec>),
}

/// Which worlds a run is carried out against.
///
/// Chosen by subcommand, since the surveying world invents answers. Its world has its own
/// identity, so a bundle that claims one is refused before replay.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Worlds {
    /// A call nothing models ends the run by name, which is §5.7.
    Ordinary,
    /// A call nothing models is answered `ENOSYS` and written down.
    Surveying,
}

impl Completed {
    /// This run as an oracle sees it, with what it produced attached.
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code) // only a build with an interceptor has implementations to compare
    )]
    pub(crate) fn as_outcome(&self) -> bohr_kernel::RunOutcome {
        bohr_kernel::RunOutcome {
            digest: self.report.digest,
            events: self.report.events,
            ticks: self.report.ticks,
            exit: None,
            killed_by: None,
            diagnostics: Vec::new(),
            projections: self
                .projected
                .iter()
                .map(|(label, value)| (label.clone(), bohr_types::Digest::of(&value.bytes)))
                .collect(),
        }
    }
}

/// One completed run and everything needed to describe or store it.
pub(crate) struct Completed {
    /// What the kernel reported.
    pub(crate) report: RunReport,
    /// The event budget the run had, recorded so a replay runs under the same one.
    pub(crate) max_events: u64,
    /// The framed event log, where it was kept in memory.
    pub(crate) log: Vec<u8>,
    /// The framed event log, where it was spooled to a file instead (`Record::Spool`).
    pub(crate) spooled: Option<bohr_trace::Spooled>,
    /// Names of the components that produced it.
    pub(crate) provenance: Provenance,
    /// Conditions under which determinism was not fully guaranteed.
    pub(crate) degradations: Vec<String>,
    /// What the run amounted to.
    pub(crate) counters: Counters,
    /// How execution was interleaved.
    pub(crate) policy: PolicyRecord,
    /// The event a finding points at.
    pub(crate) outcome_event: Option<u64>,
    /// What the guest was shown of the machine.
    pub(crate) host_profile: HostProfile,
    /// Objects the guest ran, with the fingerprints a replay checks.
    pub(crate) binaries: Vec<BinaryRecord>,
    /// What the trace writer counted while the run was recorded, so nothing reads the log back.
    pub(crate) stats: bohr_trace::TraceStats,
    /// The effective configuration, as the text a replay reconstructs the target from.
    pub(crate) config_toml: String,
    /// Records the guests wrote about themselves, one per line; empty where the log was not kept.
    pub(crate) sdk_records: Vec<u8>,
    /// Each campaign-scoped promise, as `(name, evaluations, held)`, kept or not.
    pub(crate) properties: Vec<(String, u64, u64)>,
    /// The end of what each incarnation of each node wrote out. Empty unless the log was kept.
    pub(crate) output_tails: Vec<bohr_trace::OutputTail>,
    /// What a survey found it could not model, by syscall number. Empty for an ordinary run.
    pub(crate) surveyed: Vec<(u32, u64)>,
    /// The values this run is compared by, where a differential scenario asked for any.
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code) // only a build with an interceptor has implementations to compare
    )]
    pub(crate) projected: Vec<(String, bohr_world::Value)>,
    /// The prepared filesystem the run began from, where it began from one.
    pub(crate) state_digest: Option<String>,
    /// What that filesystem held, as paths, lengths and digests.
    pub(crate) state_manifest: Option<String>,
    /// What the guest executed, where the backend could observe it.
    ///
    /// Empty for a guest built without the coverage runtime; its bundle then has no coverage
    /// section, so it is never confused with a run that covered nothing.
    pub(crate) coverage: Option<CoverageMap>,
    /// How many edges the guest was instrumented with, as the guest itself said.
    ///
    /// Kept beside the map: it describes the program, the map one run. A guest with more edges
    /// than counters has paths the search cannot fully tell apart.
    pub(crate) coverage_edges: u64,
}

/// Where a guest is told to write its records.
///
/// A path in the guest's virtual filesystem; the world treats a descriptor opened on it as the
/// record channel.
pub(crate) const SDK_RECORD_PATH: &str = "/bohr/records.jsonl";

/// Counts a run leaves, as the trace writer counted them while it recorded each event.
///
/// Counted by the writer, which sees exactly the logged events, so the log is never read back.
fn count_from_stats(stats: &bohr_trace::TraceStats, report: &RunReport) -> (Counters, Option<u64>) {
    let counters = Counters {
        events: report.events,
        syscalls: stats.syscalls,
        draws: report.journal.len() as u64,
        ticks: report.ticks.get(),
        tasks: stats.tasks.len() as u64,
        nodes: stats.nodes.len() as u64,
    };
    // A broken promise names its own event; any other outcome points at the last recorded event.
    let event = match (&report.outcome, report.violation_event) {
        (bohr_trace::Outcome::Clean, _) => None,
        (_, Some(at)) => Some(at.get()),
        (_, None) => stats.last.map(bohr_types::EventId::get),
    };
    (counters, event)
}

/// Whether the guest's own output is delivered to this process's standard output.
///
/// Only the first run of a seed prints; repetitions compare the guest's output through the trace.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// Deliver the guest's bytes.
    Deliver,
    /// Accept them without delivering.
    Discard,
}

/// Whether the event log is kept or only digested.
///
/// A repetition that agrees is never read, so it keeps only the digest. A repetition that disagrees
/// is run again with the log.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Record {
    /// Keep every event.
    Log,
    /// Keep every event, compressed into a file as it is recorded, for a run that only becomes a
    /// bundle: a campaign's finding, whose log a cluster makes too large to hold.
    Spool,
    /// Digest the events and keep none.
    DigestOnly,
}

impl Record {
    /// Whether the events are kept, in memory or in a file.
    pub(crate) const fn keeps_events(self) -> bool {
        !matches!(self, Self::DigestOnly)
    }
}

/// How execution is interleaved: the policies this build implements, each by the name the command
/// line takes and a bundle records.
///
/// Only policies that exist, so a report never names a policy that did not run.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum Scheduler {
    /// Priority-based (PCT), which guarantees which orderings it reaches.
    Pct,
    /// A uniform choice at every point.
    Random,
    /// No task passed over more than a bound.
    Fair,
}

impl Scheduler {
    /// The name the command line takes and a bundle records.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Pct => "pct",
            Self::Random => "random",
            Self::Fair => "fair",
        }
    }

    /// The policy a recorded name stands for, where this build has one.
    fn named(name: &str) -> Option<Self> {
        [Self::Pct, Self::Random, Self::Fair].into_iter().find(|s| s.name() == name)
    }

    fn policy(self) -> Box<dyn SchedulePolicy> {
        match self {
            Self::Pct => Box::new(PctPolicy::default()),
            Self::Random => Box::new(RandomPolicy),
            Self::Fair => Box::new(FairPolicy::default()),
        }
    }
}

/// The policy a recorded name stands for, where this build has one.
pub(crate) fn policy_named(name: &str) -> Option<Box<dyn SchedulePolicy>> {
    Scheduler::named(name).map(Scheduler::policy)
}

/// Everything one execution is given, as named fields.
///
/// The same eight values go to every entry point: a run, a survey, a hand-over, one
/// implementation of a comparison. Named fields stop two of them being swapped unnoticed
/// (`docs/ARCHITECTURE.md` §8, rule 5).
#[derive(Clone, Copy)]
pub(crate) struct Execution<'a> {
    /// What is run, and therefore how it is caught.
    pub(crate) target: &'a Target,
    /// The bytes every decision is drawn from.
    pub(crate) tape: &'a Tape,
    /// The bounds the run ends at.
    pub(crate) config: RunConfig,
    /// Whether what the guest writes out reaches this process's own output.
    pub(crate) output: Output,
    /// Which scheduler interleaves the tasks, by the name the command line accepts.
    pub(crate) policy: &'a str,
    /// Whether the event log is kept or only digested.
    pub(crate) record: Record,
    /// The world a scenario file asked for, where one was given.
    pub(crate) scenario: Option<&'a LoadedConfig>,
    /// That scenario's own text, which the bundle stores verbatim.
    pub(crate) config_toml: &'a str,
}

/// Runs the execution once, recording its log as the execution asks.
pub(crate) fn once(exec: Execution<'_>) -> Result<Completed, CliError> {
    run_with(exec)
}

/// Runs `target` up to the break its configuration names, then gives the guests to a debugger.
///
/// Nothing is digested, compared or written: the run only brings the world to the break state.
///
/// # Errors
///
/// Reports a backend that cannot hand anything over, and anything the run itself could not do.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(unused_variables) // a build with no interceptor refuses before it reads any of them
)]
pub(crate) fn hand_over_at(exec: Execution<'_>) -> Result<(), CliError> {
    match exec.target {
        // Already refused earlier, before the replay; the arm repeats the refusal.
        Target::Demo(..) => Err(CliError::internal(crate::handover::NO_PROCESS.to_owned())),

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Target::Program(specs) => {
            let mut backend =
                bohr_backend_ptrace::PtraceBackend::new().with_clock_page(exec.config.clock_page);
            if !backend.capabilities().hands_over_to_debugger {
                return Err(CliError::internal(
                    "BOHR-3006: this backend cannot hand a guest to a debugger".to_owned(),
                ));
            }
            let completed = drive(
                &mut backend,
                "ptrace",
                specs,
                Execution { output: Output::Discard, record: Record::DigestOnly, ..exec },
                Worlds::Ordinary,
            )?;
            let asked_for = exec.config.break_at.map(bohr_types::EventId::get).unwrap_or_default();
            let targets = backend.hand_over().map_err(|e| CliError::internal(e.to_string()))?;
            // While the backend lives: dropping it ends the released guests.
            crate::handover::conduct(&targets, completed.report.events, asked_for);
            Ok(())
        }
    }
}

/// Runs the execution once, against worlds that refuse what they cannot model.
pub(crate) fn run_with(exec: Execution<'_>) -> Result<Completed, CliError> {
    run_against(exec, Worlds::Ordinary)
}

/// Runs `target` once against worlds that answer what they cannot model instead of refusing it.
///
/// A survey runs on invented answers, so it yields a report, never a verdict, and never a bundle.
/// The log is kept only on request, as `bohr init` asks to learn which calls a program makes.
pub(crate) fn survey_with(exec: Execution<'_>) -> Result<Completed, CliError> {
    run_against(exec, Worlds::Surveying)
}

fn run_against(exec: Execution<'_>, worlds: Worlds) -> Result<Completed, CliError> {
    match exec.target {
        Target::Demo(name, program) => {
            let mut backend = NullBackend::new(program.clone());
            let specs = [GuestSpec {
                argv: vec![format!("builtin:{name}")],
                env: Vec::new(),
                executable: None,
            }];
            drive(&mut backend, "null", &specs, exec, worlds)
        }

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Target::Program(specs) => {
            let mut backend = bohr_backend_ptrace::PtraceBackend::with_coverage_bytes(
                exec.scenario
                    .and_then(|scenario| scenario.coverage_bytes)
                    .unwrap_or(bohr_types::DEFAULT_COVERAGE_MAP_BYTES),
            );
            if exec.scenario.is_some_and(|s| s.fault_policy.enomem.0 > 0) {
                backend = backend.watching_allocations();
            }
            backend = backend.with_clock_page(exec.config.clock_page);
            let completed = drive(&mut backend, "ptrace", specs, exec, worlds)?;
            // Known only once the guest has started, so folded in afterwards.
            Ok(Completed { degradations: backend.degradations().to_vec(), ..completed })
        }
    }
}

fn drive(
    backend: &mut dyn DeterminismBackend,
    backend_name: &str,
    specs: &[GuestSpec],
    exec: Execution<'_>,
    worlds: Worlds,
) -> Result<Completed, CliError> {
    let Execution {
        tape, config, output, policy: policy_name, record, scenario, config_toml, ..
    } = exec;
    // Two slots for one world; the scenario decides which model fills its slot.
    let mut minimal = bohr_world::MinimalWorld::new();
    let mut cluster = bohr_world::DistributedWorld::new();
    // Only a kept log becomes a bundle, so only then is the guests' output kept for it.
    if record.keeps_events() {
        minimal = minimal.keeping_tails();
        cluster = cluster.keeping_tails();
    }
    let world: &mut dyn bohr_kernel::WorldModel = match scenario {
        None => {
            if output == Output::Deliver {
                minimal = minimal.with_output(Box::new(std::io::stdout()));
            }
            if worlds == Worlds::Surveying {
                minimal = minimal.surveying();
            }
            // The guest's own path, from the specs, so a replay repeats it.
            if let Some(program) = specs.first().and_then(|spec| spec.argv.first()) {
                minimal = minimal.with_program(program.clone());
            }
            &mut minimal
        }
        Some(loaded) => {
            cluster = cluster
                .with_network(loaded.network.clone())
                .with_disk(loaded.disk)
                .with_node_faults(loaded.node_faults.clone())
                .with_sdk_output(SDK_RECORD_PATH.to_owned())
                .with_fault_policy(loaded.fault_policy);
            if let Some(liveness) = loaded.liveness {
                cluster = cluster.with_liveness(liveness);
            }
            if let Some(durability) = loaded.durability {
                cluster = cluster.with_durability(durability);
            }
            if loaded.linux.io_uring == bohr_world::IoUring::Ring {
                cluster = cluster.with_io_uring();
            }
            // The load and the probe follow the scenario's own nodes, as `specs_for` appends them.
            if !loaded.probes.is_empty() {
                cluster = cluster.with_probes(loaded.probes.clone(), loaded.probe_node());
            }
            if let Some(model) = loaded.model {
                cluster = cluster.with_model(model, loaded.load_node());
            }
            if let Some(state) = loaded.state.clone() {
                cluster = cluster.from_state(state, loaded.cwd.clone());
            }
            if loaded.differential.as_ref().is_some_and(|d| {
                d.projections.iter().any(|p| p.source == bohr_world::Source::Stdout)
            }) {
                cluster = cluster.keeping_output();
            }
            if output == Output::Deliver {
                cluster = cluster.with_output(Box::new(std::io::stdout()));
            }
            if worlds == Worlds::Surveying {
                cluster = cluster.surveying();
            }
            if !record.keeps_events() {
                cluster = cluster.without_sdk_records();
            }
            cluster = cluster.with_programs(
                specs.iter().map(|spec| spec.argv.first().cloned().unwrap_or_default()).collect(),
            );
            &mut cluster
        }
    };
    let mut policy = policy_named(policy_name)
        .ok_or_else(|| CliError::internal(format!("no scheduler named {policy_name}")))?;
    let mut oracle =
        bohr_world::panel(scenario.map(bohr_world::LoadedConfig::watching).unwrap_or_default());
    // An unread run is only digested; the digest comes from the event fields either way.
    let mut sink = match record {
        Record::Log | Record::Spool => TraceWriter::new(LogSink::new(record)?),
        Record::DigestOnly => TraceWriter::digest_only(LogSink::new(record)?),
    };

    let provenance = Provenance {
        world_model: bohr_kernel::WorldModel::identity(world),
        oracle: bohr_kernel::Oracle::identity(&oracle),
        backend: backend_name.to_owned(),
    };

    // The probe is the last node where the scenario asks for one, and is armed with nothing.
    let probe = scenario
        .filter(|s| !s.probes.is_empty())
        .and_then(|_| u32::try_from(specs.len().checked_sub(1)?).ok())
        .map(bohr_types::NodeId);
    let arming = scenario.map_or(bohr_kernel::Arming::NONE, |s| bohr_kernel::Arming {
        names: &s.fault_points,
        odds: s.fault_point_odds,
        except: probe,
    });
    let report = run(RunInputs {
        arming,
        tape,
        backend,
        world,
        policy: policy.as_mut(),
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs,
        config,
    })
    .map_err(|e| CliError::internal(e.to_string()))?;

    // The world's borrow ended with the run, so what it collected can be read now.
    let sdk_records = cluster.sdk_records();
    let properties = cluster.sdk_tallies();
    let projected = scenario
        .and_then(|loaded| loaded.differential.as_ref())
        .map(|differential| {
            differential
                .projections
                .iter()
                .filter_map(|spec| {
                    let value = match scenario {
                        Some(_) => cluster.project(spec),
                        // No scenario means one node; other node names were refused at load.
                        None => minimal.project(&spec.source),
                    }?;
                    Some((spec.label.clone(), value))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let state = scenario.and_then(|loaded| loaded.state.as_ref());
    let state_digest = state.map(|image| format!("blake3:{}", image.digest().to_hex()));
    let state_manifest = state.map(bohr_world::StateImage::manifest);
    let surveyed = match scenario {
        Some(_) => cluster.surveyed(),
        None => minimal.surveyed(),
    };
    let output_tails = match scenario {
        Some(_) => cluster.output_tails(),
        None => minimal.output_tails(),
    };
    let (sink, stats) = sink.into_parts();
    let (log, spooled) = sink.finish()?;
    let (counters, outcome_event) = count_from_stats(&stats, &report);
    let capabilities = backend.capabilities();
    // Read after the run, so the search never depends on when it sampled.
    let coverage = backend.coverage().filter(|map| !map.is_empty()).cloned();
    let coverage_edges = backend.coverage_edges();

    Ok(Completed {
        report,
        max_events: config.max_events,
        log,
        spooled,
        provenance,
        // Filled by whoever owns a backend that knows of any; nothing here can.
        degradations: Vec::new(),
        counters,
        policy: PolicyRecord {
            scheduler: policy_name.to_owned(),
            pct_depth: (policy_name == "pct").then_some(PCT_DEPTH),
            switch_points: SWITCH_POINTS.to_owned(),
        },
        outcome_event,
        host_profile: HostProfile {
            backend: capabilities.name.to_owned(),
            progress_unit: format!("{:?}", capabilities.progress_unit).to_lowercase(),
            page_size: PRESENTED_PAGE_SIZE,
            cpu_count_presented: 1,
            kernel_release_presented: PRESENTED_KERNEL_RELEASE.to_owned(),
            aslr_disabled: true,
            at_random_zeroed: true,
            traps_rdtsc: capabilities.traps_rdtsc,
            traps_cpuid: capabilities.traps_cpuid,
            traps_rdrand: capabilities.traps_rdrand,
        },
        // Only a kept log becomes a bundle, and only a bundle carries fingerprints.
        binaries: if record.keeps_events() { fingerprints(specs) } else { Vec::new() },
        stats,
        config_toml: config_toml.to_owned(),
        sdk_records,
        properties,
        output_tails,
        surveyed,
        projected,
        state_digest,
        state_manifest,
        coverage,
        coverage_edges,
    })
}

/// What every guest is shown, whatever the host runs.
const PRESENTED_PAGE_SIZE: u32 = 4096;

/// The kernel release the world model presents, recorded in bundles read without that world.
const PRESENTED_KERNEL_RELEASE: &str = "5.15.0-bohr";

/// Depth the default `pct` policy is built with.
const PCT_DEPTH: u32 = 3;

/// The stops the scheduler is offered a choice at, in the notation `docs/API.md` §4.2 uses.
const SWITCH_POINTS: &str = "syscall+futex";

/// What tells one build of a file from another without reading it.
type FileIdentity = (u64, Option<std::time::SystemTime>, u64, i64, i64);

/// A file's length and modification time, and on Unix its inode and the moment the inode last
/// changed, which every write moves and neither `cp -p` nor a build's fixed timestamps can set.
fn file_identity(meta: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    let (inode, changed, changed_ns) = {
        use std::os::unix::fs::MetadataExt as _;
        (meta.ino(), meta.ctime(), meta.ctime_nsec())
    };
    #[cfg(not(unix))]
    let (inode, changed, changed_ns) = (0, 0, 0);
    (meta.len(), meta.modified().ok(), inode, changed, changed_ns)
}

/// Fingerprints of the programs a run was started with.
///
/// Only what the supervisor was asked to run; objects mapped later are known only to the backend.
fn fingerprints(specs: &[GuestSpec]) -> Vec<BinaryRecord> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    // Cached for the process: a worker saves many bundles of one guest, and hashing the binary is
    // a large share of a short run. Keyed by what a rebuild changes, so a rebuild is rehashed.
    type Known = std::collections::BTreeMap<(String, FileIdentity), String>;
    static KNOWN: std::sync::Mutex<Known> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    for spec in specs {
        let Some(path) = spec.argv.first() else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        // A built-in guest has a name and no file; the manifest's tool version covers it.
        // A program is hashed where it runs from, so the record names the bytes that ran: the
        // probe build that judged the bundle, or a file moved with `--binaries`.
        let file = spec.executable.as_ref().unwrap_or(path);
        let blake3 = match path.strip_prefix("builtin:") {
            Some(_) => String::new(),
            None => {
                let Ok(meta) = std::fs::metadata(file) else { continue };
                let key = (file.clone(), file_identity(&meta));
                let cached = KNOWN.lock().ok().and_then(|known| known.get(&key).cloned());
                match cached {
                    Some(digest) => digest,
                    None => match std::fs::read(file) {
                        Ok(bytes) => {
                            let digest = bohr_types::Digest::of(&bytes).to_string();
                            if let Ok(mut known) = KNOWN.lock() {
                                known.insert(key, digest.clone());
                            }
                            digest
                        }
                        Err(_) => continue,
                    },
                }
            }
        };
        // The probe is known by its name inside the run: a replay with `--binaries` also runs
        // the scenario's programs from a file other than `argv[0]`.
        let supplied = path == crate::PROBE_ARGV0 || path == crate::LOAD_ARGV0;
        let role = match (blake3.is_empty(), supplied) {
            (true, _) => bohr_trace::BinaryRole::Builtin,
            (false, true) => bohr_trace::BinaryRole::Probe,
            (false, false) => bohr_trace::BinaryRole::Executable,
        };
        out.push(BinaryRecord {
            path: path.clone(),
            blake3,
            role,
            // What the program was started with, so a replay starts it the same way.
            args: spec.argv.iter().skip(1).cloned().collect(),
        });
    }
    out
}

/// Collects the event log in memory, spools it to a file, or counts it and keeps nothing.
enum LogSink {
    Memory(Vec<u8>),
    Spool(Box<bohr_trace::Spool>),
    Discard,
}

impl LogSink {
    fn new(record: Record) -> Result<Self, CliError> {
        Ok(match record {
            Record::Log => Self::Memory(Vec::new()),
            Record::Spool => {
                // A name another holds is passed over for the next.
                let mut tried = 0;
                let spool = loop {
                    let path = spool_path();
                    match bohr_trace::Spool::create(&path) {
                        Ok(spool) => break spool,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tried < 100 => {
                            tried += 1;
                        }
                        Err(e) => {
                            return Err(CliError::usage(format!(
                                "BOHR-1002: cannot write {}: {e}",
                                path.display()
                            )));
                        }
                    }
                };
                Self::Spool(Box::new(spool))
            }
            Record::DigestOnly => Self::Discard,
        })
    }

    /// The log kept in memory, and the spool where there is one instead.
    fn finish(self) -> Result<(Vec<u8>, Option<bohr_trace::Spooled>), CliError> {
        match self {
            Self::Memory(bytes) => Ok((bytes, None)),
            Self::Spool(spool) => {
                let spooled = spool.finish().map_err(|e| {
                    CliError::usage(format!("BOHR-1002: cannot finish the event spool: {e}"))
                })?;
                Ok((Vec::new(), Some(spooled)))
            }
            Self::Discard => Ok((Vec::new(), None)),
        }
    }
}

/// A file of this process's own for one run's spooled events, in the temporary directory.
///
/// Named by process and a counter, since several workers spool at once; the name never reaches
/// the run.
fn spool_path() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("bohr-events-{}-{n}.zst", std::process::id()))
}

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Memory(bytes) => bytes.extend_from_slice(buf),
            Self::Spool(spool) => spool.write_all(buf)?,
            Self::Discard => {}
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Spool(spool) => spool.flush(),
            Self::Memory(_) | Self::Discard => Ok(()),
        }
    }
}

/// Where two runs of the same seed stopped agreeing.
pub(crate) struct Divergence {
    /// Which repetition diverged, counted from one.
    pub(crate) attempt: usize,
    /// Position in the event stream.
    pub(crate) index: usize,
    /// What the first run recorded there, absent if it ended sooner.
    pub(crate) reference: Option<Event>,
    /// What this run recorded there, absent if it ended sooner.
    pub(crate) actual: Option<Event>,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "runs diverged at event {} on attempt {}", self.index, self.attempt)?;
        match &self.reference {
            Some(e) => write!(f, "  first run: {}", bohr_report::text::event_line(e))?,
            None => writeln!(f, "  first run: ended before this event")?,
        }
        match &self.actual {
            Some(e) => write!(f, "  this run:  {}", bohr_report::text::event_line(e).trim_end()),
            None => write!(f, "  this run:  ended before this event"),
        }
    }
}

/// Runs `target` `times` times under one seed and checks every run agrees; the first run's output
/// goes where `exec` says, every repetition's nowhere.
///
/// Reports the first differing event, which is more useful than "digests differ".
pub(crate) fn repeat(
    exec: Execution<'_>,
    times: usize,
) -> Result<Result<Completed, Divergence>, CliError> {
    let first = once(exec)?;
    if times <= 1 {
        return Ok(Ok(first));
    }
    // Decoded only when there are repetitions to compare; decoding a long log is expensive.
    let reference: Vec<Event> =
        read_events(&first.log).map_err(|e| CliError::internal(e.to_string()))?;

    // Repetitions are checks; their output is discarded.
    let quiet = Execution { output: Output::Discard, ..exec };
    for attempt in 2..=times {
        let again = run_with(Execution { record: Record::DigestOnly, ..quiet })?;
        if again.report.digest == first.report.digest {
            continue;
        }
        // The disagreeing run had no log, so it runs again with one. If that run agrees, the
        // divergence is not in the tape, and the comparison reports an empty difference.
        let again = once(quiet)?;
        let actual = read_events(&again.log).map_err(|e| CliError::internal(e.to_string()))?;
        let index = (0..reference.len().max(actual.len()))
            .find(|i| reference.get(*i) != actual.get(*i))
            .unwrap_or(0);
        return Ok(Err(Divergence {
            attempt,
            index,
            reference: reference.get(index).cloned(),
            actual: actual.get(index).cloned(),
        }));
    }

    Ok(Ok(first))
}

/// How many syscalls a run intercepted, and which numbers dominated.
///
/// The first place to look when a run gets slower: a jump usually means a syscall moved from
/// admitted to traced. Taken from the trace writer's counts.
pub(crate) fn syscall_profile(
    stats: &bohr_trace::TraceStats,
    take: usize,
) -> (u64, Vec<(u32, u64)>) {
    let total = stats.syscalls;
    let mut ranked: Vec<(u32, u64)> = stats
        .by_nr
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(nr, count)| (nr as u32, *count))
        .collect();
    ranked.sort_by_key(|(nr, count)| (std::cmp::Reverse(*count), *nr));
    ranked.truncate(take);
    (total, ranked)
}

/// The last diagnostics a run recorded, most recent last.
///
/// What the supervisor noticed on the way to the outcome, which usually explains it.
pub(crate) fn last_diagnostics(stats: &bohr_trace::TraceStats, take: usize) -> Vec<String> {
    let mut seen: Vec<String> =
        stats.diagnostics.iter().map(|(code, message)| format!("{code}: {message}")).collect();
    seen.dedup();
    if seen.len() > take {
        seen.drain(..seen.len() - take);
    }
    seen
}

/// Serialises a completed run as a scenario bundle.
///
/// Every section a replay needs is written, empty ones included: an absent `Binaries` section and
/// an empty one mean different things to a replay.
pub(crate) fn to_bundle(
    completed: &Completed,
    tape: &Tape,
    reduced_from: Option<String>,
) -> Result<Vec<u8>, CliError> {
    let (writer, run_id) = bundle_writer(completed, tape, reduced_from)?;
    Ok(writer.finish(run_id))
}

/// Writes a completed run as a scenario bundle at `path`, streaming a spooled event log from its
/// file, so the log is never held whole.
pub(crate) fn write_bundle(
    completed: &Completed,
    tape: &Tape,
    reduced_from: Option<String>,
    path: &std::path::Path,
) -> Result<(), CliError> {
    let refused = |e: std::io::Error| {
        CliError::usage(format!("BOHR-1002: cannot write {}: {e}", path.display()))
    };
    let (writer, run_id) = bundle_writer(completed, tape, reduced_from)?;
    // Written beside and renamed, so a campaign stopped mid-copy leaves no bundle that looks saved.
    let part = path.with_extension("bohr.part");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&part).map_err(refused)?);
    writer.write_to(&mut file, run_id).map_err(refused)?;
    std::io::Write::flush(&mut file).map_err(refused)?;
    drop(file);
    std::fs::rename(&part, path).map_err(refused)
}

/// Every section of a completed run's bundle, and the bundle's run identifier.
fn bundle_writer(
    completed: &Completed,
    tape: &Tape,
    reduced_from: Option<String>,
) -> Result<(BundleWriter, [u8; 16]), CliError> {
    let run_id = derive_run_id(tape.seed(), completed.report.digest);
    let manifest = manifest_for(bohr_trace::ManifestInputs {
        reduced_from,
        state_digest: completed.state_digest.clone(),
        max_events: Some(completed.max_events),
        seed: tape.seed(),
        trace_digest: completed.report.digest,
        run_id,
        provenance: completed.provenance.clone(),
        outcome: completed.report.outcome.clone(),
        outcome_event: completed.outcome_event,
        counters: completed.counters,
        policy: completed.policy.clone(),
        degraded_reasons: completed.degradations.clone(),
        created_utc: bohr_trace::now_utc(),
    });
    let mut writer = BundleWriter::new();
    writer
        .section(
            SectionId::Manifest,
            serde_json::to_vec_pretty(&manifest).map_err(|e| CliError::internal(e.to_string()))?,
        )
        .section(SectionId::Tape, tape.to_container())
        .section(SectionId::DrawJournal, completed.report.journal.to_bytes())
        .section(
            SectionId::HostProfile,
            serde_json::to_vec_pretty(&completed.host_profile)
                .map_err(|e| CliError::internal(e.to_string()))?,
        )
        .section(
            SectionId::Binaries,
            serde_json::to_vec_pretty(&completed.binaries)
                .map_err(|e| CliError::internal(e.to_string()))?,
        )
        .section(SectionId::Config, completed.config_toml.clone().into_bytes());
    match &completed.spooled {
        Some(spooled) => writer.spooled_section(SectionId::Events, spooled),
        None => writer.section(SectionId::Events, completed.log.clone()),
    };
    if !completed.sdk_records.is_empty() {
        writer.section(SectionId::SdkRecords, completed.sdk_records.clone());
    }
    if !completed.output_tails.is_empty() {
        writer.section(
            SectionId::GuestOutput,
            serde_json::to_vec(&completed.output_tails)
                .map_err(|e| CliError::internal(e.to_string()))?,
        );
    }
    // The file list only; real state runs to gigabytes.
    if let Some(manifest) = &completed.state_manifest {
        writer.section(SectionId::StateManifest, manifest.clone().into_bytes());
    }
    // Only when something was recorded; an uninstrumented guest would add 64 KB of zeroes.
    if let Some(map) = &completed.coverage {
        writer.section(SectionId::Coverage, map.as_bytes().to_vec());
    }
    Ok((writer, run_id))
}

/// One tape, several implementations, and what a group oracle made of them.
///
/// The implementations run one after another on the same tape; the kernel is unchanged. What each
/// produced is compared, never their event streams, which differ by nature.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code) // only a build with an interceptor has implementations to compare
)]
pub(crate) struct Group {
    /// Each implementation's run, in the order the configuration named them.
    pub(crate) executions: Vec<Completed>,
    /// What each is called.
    pub(crate) names: Vec<String>,
    /// What the group oracle made of them together.
    pub(crate) verdict: bohr_kernel::Verdict,
}

/// Runs every implementation a differential scenario names, and asks whether they agree.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code, unused_variables) // only a build with an interceptor compares anything
)]
pub(crate) fn compare(
    program: Vec<String>,
    tape: &Tape,
    config: RunConfig,
    policy: &str,
    scenario: &LoadedConfig,
    config_toml: &str,
) -> Result<Group, CliError> {
    let Some(differential) = scenario.differential.as_ref() else {
        return Err(CliError::usage(
            "BOHR-1002: this scenario names no implementations to compare".to_owned(),
        ));
    };

    let mut executions = Vec::new();
    let mut names = Vec::new();
    for implementation in &differential.implementations {
        let target = implementation_target(program.clone(), scenario, implementation)?;
        // The same tape from offset zero for each. Implementations make different calls, so their
        // draws land differently.
        executions.push(run_with(Execution {
            target: &target,
            tape,
            config,
            output: Output::Discard,
            policy,
            record: Record::Log,
            scenario: Some(scenario),
            config_toml,
        })?);
        names.push(implementation.name.clone());
    }

    let outcomes: Vec<bohr_kernel::RunOutcome> =
        executions.iter().map(Completed::as_outcome).collect();
    let mut oracle = bohr_world::AgreementOracle;
    let verdict = bohr_world::GroupOracle::judge_group(&mut oracle, &outcomes);

    Ok(Group { executions, names, verdict })
}

/// The programs one implementation runs, where this host can intercept them.
///
/// The whole cluster, with this implementation in the place of the one node it stands in for.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn implementation_target(
    argv: Vec<String>,
    scenario: &LoadedConfig,
    implementation: &bohr_world::Implementation,
) -> Result<Target, CliError> {
    Ok(Target::Program(crate::specs_for(argv, Some(scenario), Some(implementation))?))
}

/// Refuses on a host with no interceptor, for the same reason `bohr run` does.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn implementation_target(
    _argv: Vec<String>,
    _scenario: &LoadedConfig,
    _implementation: &bohr_world::Implementation,
) -> Result<Target, CliError> {
    Err(CliError::usage(
        "BOHR-3005: this build has no interceptor, so there are no implementations to compare"
            .to_owned(),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A rebuild that keeps the old length and the old modification time is hashed again, as a
    /// build with fixed timestamps leaves them.
    #[test]
    fn a_rebuild_that_keeps_length_and_time_is_hashed_again() {
        let dir = std::env::temp_dir().join(format!("bohr-fingerprint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("guest");
        std::fs::write(&path, b"first build").expect("written");
        let when = std::fs::metadata(&path).and_then(|m| m.modified()).expect("a time");
        let spec =
            GuestSpec { argv: vec![path.to_string_lossy().into_owned()], ..GuestSpec::default() };
        let first = fingerprints(std::slice::from_ref(&spec));
        // Past the kernel's clock tick, which the inode's change time moves by; a rebuild is
        // seconds later.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"other build").expect("rebuilt, one length");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(when))
            .expect("the old time put back");
        let second = fingerprints(std::slice::from_ref(&spec));
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(first[0].blake3, second[0].blake3, "the old build's digest was kept");
    }
}
