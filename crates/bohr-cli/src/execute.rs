//! Running a guest, and comparing runs against each other.

use bohr_backend::{CoverageMap, DeterminismBackend, GuestSpec};
use bohr_backend_null::{NullBackend, NullProgram};
use bohr_kernel::{run, PctPolicy, RandomPolicy, RunConfig, RunInputs, RunReport, SchedulePolicy};
use bohr_tape::Tape;
use bohr_trace::{
    derive_run_id, manifest_for, read_events, read_events_lossy, BinaryRecord, BundleWriter,
    Counters, Event, HostProfile, PolicyRecord, Provenance, SectionId, TraceWriter,
};
use bohr_world::LoadedConfig;

use crate::CliError;

/// What to run.
pub(crate) enum Target {
    /// A guest built into the binary, executed in process, and the name it answers to.
    ///
    /// Named as well as carried, because a bundle has to say which guest to replay: a program on
    /// disk is identified by its path, and a built-in one has none.
    Demo(String, NullProgram),
    /// Programs on disk — one per node — executed under interception.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Program(Vec<GuestSpec>),
}

/// Which worlds a run is carried out against.
///
/// Not a flag on any command a person types: the two answer different questions and one of them
/// invents answers. What keeps them apart for a user is that `bohr survey` is its own subcommand,
/// and what keeps them apart afterwards is that a surveying world has an identity of its own, so
/// a bundle claiming one is refused before it replays.
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
    /// The framed event log.
    pub(crate) log: Vec<u8>,
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
    /// The effective configuration, as the text a replay reconstructs the target from.
    pub(crate) config_toml: String,
    /// Records the guests wrote about themselves, one per line.
    pub(crate) sdk_records: Vec<u8>,
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
    /// Empty for a guest built without the coverage runtime, which is a run searched without
    /// coverage rather than a run that covered nothing — the two are opposite facts and the
    /// section is absent for the first so that nobody has to tell them apart by reading zeroes.
    pub(crate) coverage: Option<CoverageMap>,
    /// How many edges the guest was instrumented with, as the guest itself said.
    ///
    /// Beside the map rather than inside it, because it is a fact about the program and the map is
    /// a fact about one run of it. What it is for is the comparison: a guest with more edges than
    /// the map has counters is a guest whose paths the search cannot fully tell apart.
    pub(crate) coverage_edges: u64,
}

/// Where a guest is told to write its records.
///
/// A path inside the guest's own virtual filesystem: nothing reaches the host, and the world
/// recognises a descriptor opened on it as the record channel rather than as a file.
pub(crate) const SDK_RECORD_PATH: &str = "/bohr/records.jsonl";

/// Counts a run leaves in its own event log.
///
/// Read back from the log rather than threaded through the kernel: the log is what a bundle
/// carries, so a counter derived from it can never disagree with what a reader would count.
fn count_from_log(log: &[u8], report: &RunReport) -> (Counters, Option<u64>) {
    let (events, _) = read_events_lossy(log);
    let mut syscalls = 0;
    let mut tasks = std::collections::BTreeSet::new();
    let mut nodes = std::collections::BTreeSet::new();
    for event in &events {
        nodes.insert(event.node);
        tasks.insert(event.task);
        if matches!(event.kind, bohr_trace::EventKind::Syscall { .. }) {
            syscalls += 1;
        }
    }
    let counters = Counters {
        events: report.events,
        syscalls,
        draws: report.journal.len() as u64,
        ticks: report.ticks.get(),
        tasks: tasks.len() as u64,
        nodes: nodes.len() as u64,
    };
    // A broken promise names its own event; anything else points at the last thing the run
    // recorded, which for a crash or a hang is where the outcome was reached. The two are
    // different questions and the run answers the first one itself.
    let event = match (&report.outcome, report.violation_event) {
        (bohr_trace::Outcome::Clean, _) => None,
        (_, Some(at)) => Some(at.get()),
        (_, None) => events.last().map(|e| e.id.get()),
    };
    (counters, event)
}

/// Whether the guest's own output is delivered to this process's standard output.
///
/// A repetition beyond the first is a check, not an execution the user asked to see, so only one
/// run of a seed ever prints. What the guest wrote is compared through the trace, not the terminal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// Deliver the guest's bytes.
    Deliver,
    /// Accept them without delivering.
    Discard,
}

/// Whether the event log is kept or only digested.
///
/// A repetition that agrees with the first is never read, and a thousand of them is a gigabyte
/// written and thrown away. The digest is maintained either way, so a run that disagrees is still
/// noticed — and then repeated with the log, which is the only time it is needed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Record {
    /// Keep every event.
    Log,
    /// Digest the events and keep none.
    DigestOnly,
}

/// How execution is interleaved, by the name the command line accepts.
///
/// A policy that does not exist is refused before a run starts rather than substituted, so a report
/// never names a policy that did not run.
pub(crate) fn policy_named(name: &str) -> Option<Box<dyn SchedulePolicy>> {
    match name {
        "pct" => Some(Box::new(PctPolicy::default())),
        "random" => Some(Box::new(RandomPolicy)),
        _ => None,
    }
}

/// Everything one execution is given, named rather than positional.
///
/// One value because the same eight things travel together through every entry point — an ordinary
/// run, a survey, a hand-over, one implementation of a comparison — and each of those would
/// otherwise repeat the whole list. Named because a signature of eight positional parameters, four
/// of which are small enumerations, is a signature two arguments can be swapped in without the
/// compiler having anything to say about it (`docs/ARCHITECTURE.md` §8, rule 5).
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

/// Runs the execution once, keeping its event log.
pub(crate) fn once(exec: Execution<'_>) -> Result<Completed, CliError> {
    run_with(Execution { record: Record::Log, ..exec })
}

/// Runs `target` up to the break its configuration names, then gives the guests to a debugger.
///
/// Separate from the ordinary run because what happens at the end is the opposite of it: nothing
/// is digested, nothing is compared and nothing is written. The run is a way of getting the world
/// into a particular state, and the state is the deliverable.
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
        // Refused earlier, where it can be said before a replay is paid for; kept because the
        // arm has to exist and an unreachable one that lies is worse than one that repeats.
        Target::Demo(..) => Err(CliError::Internal(crate::handover::NO_PROCESS.to_owned())),

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Target::Program(specs) => {
            let mut backend = bohr_backend_ptrace::PtraceBackend::new();
            if !backend.capabilities().hands_over_to_debugger {
                return Err(CliError::Internal(
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
            let targets = backend.hand_over().map_err(|e| CliError::Internal(e.to_string()))?;
            // While the backend is still alive: dropping it is what ends the guests, and it is
            // the only thing that knows they exist once they have been released.
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
/// Everything a survey learns it learns from answers that were invented, so what this produces is
/// a report and never a verdict. The worlds say so themselves: their identity differs, and a
/// bundle claiming one is refused before it replays.
pub(crate) fn survey_with(exec: Execution<'_>) -> Result<Completed, CliError> {
    run_against(Execution { record: Record::DigestOnly, ..exec }, Worlds::Surveying)
}

fn run_against(exec: Execution<'_>, worlds: Worlds) -> Result<Completed, CliError> {
    match exec.target {
        Target::Demo(name, program) => {
            let mut backend = NullBackend::new(program.clone());
            let specs = [GuestSpec { argv: vec![format!("builtin:{name}")], env: Vec::new() }];
            drive(&mut backend, "null", &specs, exec, worlds)
        }

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Target::Program(specs) => {
            let mut backend = bohr_backend_ptrace::PtraceBackend::with_coverage_bytes(
                exec.scenario
                    .and_then(|scenario| scenario.coverage_bytes)
                    .unwrap_or(bohr_backend::DEFAULT_COVERAGE_MAP_BYTES),
            );
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
    // Two slots, one world: the scenario decides which model runs, and the borrow lives in
    // whichever slot was filled.
    let mut minimal = bohr_world::MinimalWorld::new();
    let mut cluster = bohr_world::DistributedWorld::new();
    let world: &mut dyn bohr_kernel::WorldModel = match scenario {
        None => {
            if output == Output::Deliver {
                minimal = minimal.with_output(Box::new(std::io::stdout()));
            }
            if worlds == Worlds::Surveying {
                minimal = minimal.surveying();
            }
            // What the guest answers when it asks where it is. Taken from the specs rather than
            // from the host: a replay is handed the same argument list, so the answer repeats.
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
            cluster = cluster.with_programs(
                specs.iter().map(|spec| spec.argv.first().cloned().unwrap_or_default()).collect(),
            );
            &mut cluster
        }
    };
    let mut policy = policy_named(policy_name)
        .ok_or_else(|| CliError::Internal(format!("no scheduler named {policy_name}")))?;
    let mut oracle =
        bohr_world::panel(scenario.map(bohr_world::LoadedConfig::watching).unwrap_or_default());
    let mut sink = TraceWriter::new(LogSink::new(record));

    let provenance = Provenance {
        world_model: bohr_kernel::WorldModel::identity(world),
        oracle: bohr_kernel::Oracle::identity(&oracle),
        backend: backend_name.to_owned(),
    };

    let report = run(RunInputs {
        tape,
        backend,
        world,
        policy: policy.as_mut(),
        oracle: &mut oracle,
        sink: &mut sink,
        specs,
        config,
    })
    .map_err(|e| CliError::Internal(e.to_string()))?;

    // The world's borrow ended with the run, so what it collected can be read now.
    let sdk_records = cluster.sdk_records();
    let projected = scenario
        .and_then(|loaded| loaded.differential.as_ref())
        .map(|differential| {
            differential
                .projections
                .iter()
                .filter_map(|spec| {
                    let value = match scenario {
                        Some(_) => cluster.project(spec),
                        // No scenario is one node, and a source naming another was refused at
                        // load, so there is nothing here to resolve.
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
    let (sink, _, _) = sink.finish().map_err(|e| CliError::Internal(e.to_string()))?;
    let log = sink.into_bytes();
    let (counters, outcome_event) = count_from_log(&log, &report);
    let capabilities = backend.capabilities();
    // Read now that the run is over, which is the only moment it may be read: sampling while the
    // guest was running would make what a search does next depend on when it looked.
    let coverage = backend.coverage().filter(|map| !map.is_empty()).cloned();
    let coverage_edges = backend.coverage_edges();

    Ok(Completed {
        report,
        log,
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
        binaries: fingerprints(specs),
        config_toml: config_toml.to_owned(),
        sdk_records,
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

/// The kernel release the world model presents. Recorded here as well because a bundle is read
/// without the world that wrote it.
const PRESENTED_KERNEL_RELEASE: &str = "5.15.0-bohr";

/// Depth the default `pct` policy is built with.
const PCT_DEPTH: u32 = 3;

/// The stops the scheduler is offered a choice at, in the notation `docs/API.md` §4.2 uses.
const SWITCH_POINTS: &str = "syscall+futex";

/// Fingerprints of the programs a run was started with.
///
/// Only what the supervisor was asked to run: the objects a guest maps afterwards are known to
/// the backend, not here, and recording a path without a hash would be worse than recording
/// nothing at all.
fn fingerprints(specs: &[GuestSpec]) -> Vec<BinaryRecord> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for spec in specs {
        let Some(path) = spec.argv.first() else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        // A built-in guest is part of this binary: it has a name and no file, and its
        // fingerprint is the tool's own version, checked by the manifest rather than here.
        let blake3 = match path.strip_prefix("builtin:") {
            Some(_) => String::new(),
            None => match std::fs::read(path) {
                Ok(bytes) => bohr_types::Digest::of(&bytes).to_string(),
                Err(_) => continue,
            },
        };
        let role = if blake3.is_empty() { "builtin" } else { "executable" };
        out.push(BinaryRecord {
            path: path.clone(),
            blake3,
            role: role.to_owned(),
            // What the program was started with, so a replay starts it the same way.
            args: spec.argv.iter().skip(1).cloned().collect(),
        });
    }
    out
}

/// Collects the event log, or counts it and keeps nothing.
struct LogSink {
    bytes: Option<Vec<u8>>,
}

impl LogSink {
    const fn new(record: Record) -> Self {
        Self {
            bytes: match record {
                Record::Log => Some(Vec::new()),
                Record::DigestOnly => None,
            },
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes.unwrap_or_default()
    }
}

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(bytes) = self.bytes.as_mut() {
            bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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
            Some(e) => writeln!(f, "  first run: {:?}", e.kind)?,
            None => writeln!(f, "  first run: ended before this event")?,
        }
        match &self.actual {
            Some(e) => write!(f, "  this run:  {:?}", e.kind),
            None => write!(f, "  this run:  ended before this event"),
        }
    }
}

/// Runs `target` `times` times under one seed and checks every run agrees.
///
/// Reports the first event where a run differs rather than that the digests differ: the difference
/// is the whole diagnosis, and "digests differ" leaves the reader with nowhere to go.
pub(crate) fn repeat(
    exec: Execution<'_>,
    times: usize,
) -> Result<Result<Completed, Divergence>, CliError> {
    let first = once(Execution { output: Output::Deliver, ..exec })?;
    let reference: Vec<Event> =
        read_events(&first.log).map_err(|e| CliError::Internal(e.to_string()))?;

    // Every repetition beyond the first is a check rather than an execution anybody asked to see.
    let quiet = Execution { output: Output::Discard, ..exec };
    for attempt in 2..=times {
        let again = run_with(Execution { record: Record::DigestOnly, ..quiet })?;
        if again.report.digest == first.report.digest {
            continue;
        }
        // The run that disagreed was not recorded, so it is repeated for its log. A repetition of a
        // repetition that then agrees would mean the divergence is not in the tape at all, and the
        // comparison below reports that as an empty difference rather than inventing one.
        let again = once(quiet)?;
        let actual = read_events(&again.log).map_err(|e| CliError::Internal(e.to_string()))?;
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
/// Interception is what a run pays for, so this is the first thing to look at when one is slower
/// than it was: a jump here is a syscall that moved from admitted to traced, which is a change in
/// the table rather than in the machine.
///
/// Read back from the log rather than counted during the run, which keeps the counting out of the
/// path every syscall takes.
pub(crate) fn syscall_profile(log: &[u8], take: usize) -> (u64, Vec<(u32, u64)>) {
    let mut counts: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
    let (events, _) = bohr_trace::read_events_lossy(log);
    let mut total = 0u64;
    for event in events {
        if let bohr_trace::EventKind::Syscall { nr, .. } = event.kind {
            *counts.entry(nr).or_default() += 1;
            total += 1;
        }
    }
    let mut ranked: Vec<(u32, u64)> = counts.into_iter().collect();
    ranked.sort_by_key(|(nr, count)| (std::cmp::Reverse(*count), *nr));
    ranked.truncate(take);
    (total, ranked)
}

/// The last diagnostics a run recorded, most recent last.
///
/// A run that ended badly says what went wrong in its outcome; these say what the supervisor
/// noticed on the way there, which is usually where the answer is.
pub(crate) fn last_diagnostics(log: &[u8], take: usize) -> Vec<String> {
    let (events, _) = bohr_trace::read_events_lossy(log);
    let mut seen: Vec<String> = events
        .into_iter()
        .filter_map(|e| match e.kind {
            bohr_trace::EventKind::Diagnostic { code, message } => {
                Some(format!("{code}: {message}"))
            }
            _ => None,
        })
        .collect();
    seen.dedup();
    if seen.len() > take {
        seen.drain(..seen.len() - take);
    }
    seen
}

/// Serialises a completed run as a scenario bundle.
///
/// Every section a replay needs is written, including the ones that only ever say "nothing here":
/// an absent `Binaries` and an empty one mean different things, and a replay that guessed which it
/// was looking at would skip a fingerprint check without saying so.
pub(crate) fn to_bundle(
    completed: &Completed,
    tape: &Tape,
    reduced_from: Option<String>,
) -> Result<Vec<u8>, CliError> {
    let run_id = derive_run_id(tape.seed(), completed.report.digest);
    let manifest = manifest_for(bohr_trace::ManifestInputs {
        reduced_from,
        state_digest: completed.state_digest.clone(),
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
            serde_json::to_vec_pretty(&manifest).map_err(|e| CliError::Internal(e.to_string()))?,
        )
        .section(SectionId::Tape, tape.to_container())
        .section(SectionId::Events, completed.log.clone())
        .section(SectionId::DrawJournal, completed.report.journal.to_bytes())
        .section(
            SectionId::HostProfile,
            serde_json::to_vec_pretty(&completed.host_profile)
                .map_err(|e| CliError::Internal(e.to_string()))?,
        )
        .section(
            SectionId::Binaries,
            serde_json::to_vec_pretty(&completed.binaries)
                .map_err(|e| CliError::Internal(e.to_string()))?,
        )
        .section(SectionId::Config, completed.config_toml.clone().into_bytes());
    if !completed.sdk_records.is_empty() {
        writer.section(SectionId::SdkRecords, completed.sdk_records.clone());
    }
    // The list and never the content: what a bundle owes a reader is which state it needs, and a
    // real one is measured in gigabytes.
    if let Some(manifest) = &completed.state_manifest {
        writer.section(SectionId::StateManifest, manifest.clone().into_bytes());
    }
    // Present only where there was something to record. A section of sixty-four kilobytes of
    // zeroes in every bundle from an uninstrumented guest would cost more than it says.
    if let Some(map) = &completed.coverage {
        writer.section(SectionId::Coverage, map.as_bytes().to_vec());
    }
    Ok(writer.finish(run_id))
}

/// One tape, several implementations, and what a group oracle made of them.
///
/// Sequential rather than interleaved, and that is the whole shape of the feature: the kernel does
/// not change at all. Each implementation is run on its own against the same bytes, and what is
/// compared afterwards is what each produced — never their event streams, which cannot match and
/// were never meant to.
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
        return Err(CliError::Usage(
            "BOHR-1002: this scenario names no implementations to compare".to_owned(),
        ));
    };

    let mut executions = Vec::new();
    let mut names = Vec::new();
    for implementation in &differential.implementations {
        let target = implementation_target(program.clone(), scenario, implementation)?;
        // The same tape, from offset zero, for each. What that shares is one source of adversity
        // and not one schedule: two implementations make different calls in a different order, so
        // every draw whose position depends on the guest lands somewhere else for each of them.
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
    let verdict = bohr_kernel::Oracle::judge_group(&mut oracle, &outcomes);

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
    Ok(Target::Program(crate::specs_for(argv, Some(scenario), Some(implementation))))
}

/// Refuses on a host with no interceptor, for the same reason `bohr run` does.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn implementation_target(
    _argv: Vec<String>,
    _scenario: &LoadedConfig,
    _implementation: &bohr_world::Implementation,
) -> Result<Target, CliError> {
    Err(CliError::Usage(
        "BOHR-3005: this build has no interceptor, so there are no implementations to compare"
            .to_owned(),
    ))
}
