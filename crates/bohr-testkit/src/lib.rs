//! Fixtures and the harness the determinism suite is built on.
//!
//! The guests here are scripted rather than compiled: they run in process, on any operating system,
//! with no privileges, so the clock, the scheduler, the world and the trace can be tested without
//! anything being intercepted. They are also what `bohr selftest` runs, which keeps the guests a
//! user can invoke and the guests continuous integration checks the same guests.

#![forbid(unsafe_code)]

pub mod violation;

use bohr_backend::{CoverageMap, DeterminismBackend as _, GuestSpec};
use bohr_backend_null::{GuestOp, MEMORY_BASE, NullBackend, NullProgram};
use bohr_explorer::{RunResult, WorkItem};
use bohr_kernel::{KernelError, RandomPolicy, RunConfig, RunInputs, RunReport, WorldModel, run};
use bohr_tape::Tape;
use bohr_trace::{Event, TraceWriter, read_events};
use bohr_types::{Digest, GuestAddr, NodeId, Seed};
use bohr_world::{DistributedWorld, MinimalWorld};

/// Linux x86-64 syscall numbers the scripted guests use, matching what a real guest would issue.
mod sys {
    /// `read`.
    pub(crate) const READ: u32 = 0;
    /// `write`.
    pub(crate) const WRITE: u32 = 1;
    /// `clock_gettime`.
    pub(crate) const CLOCK_GETTIME: u32 = 228;
    /// `getrandom`.
    pub(crate) const GETRANDOM: u32 = 318;
    /// `futex`.
    pub(crate) const FUTEX: u32 = 202;
    /// `getpid`.
    pub(crate) const GETPID: u32 = 39;
    /// `gettid`.
    pub(crate) const GETTID: u32 = 186;
    /// `sched_yield`.
    pub(crate) const SCHED_YIELD: u32 = 24;
    /// `ioctl`.
    pub(crate) const IOCTL: u32 = 16;
    /// `socket`.
    pub(crate) const SOCKET: u32 = 41;
    /// `sendto`.
    pub(crate) const SENDTO: u32 = 44;
    /// `recvfrom`.
    pub(crate) const RECVFROM: u32 = 45;
    /// `bind`.
    pub(crate) const BIND: u32 = 49;
    /// `clock_nanosleep`.
    pub(crate) const CLOCK_NANOSLEEP: u32 = 230;
    /// `openat`.
    pub(crate) const OPENAT: u32 = 257;
}

/// `futex` operations the fixtures use.
mod futex_op {
    /// Block while the word still holds the expected value.
    pub(crate) const WAIT: u64 = 0;
    /// Release waiters.
    pub(crate) const WAKE: u64 = 1;
}

/// A scripted guest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fixture {
    /// Reads the clock and asks for randomness.
    ClockAndRandom,
    /// Two tasks interleaving, each reading the clock and asking for randomness.
    TwoTasks,
    /// Asks for a page of randomness in one call.
    LargeRandom,
    /// Two tasks wait on one word and a third releases them both.
    ///
    /// Which of the two goes first is a decision, and the whole point of the fixture is that it
    /// comes from the tape rather than from the order they happened to arrive in.
    ///
    /// Absent from [`Fixture::all`]: a scripted guest cannot make the waits happen before the
    /// wake, so an interleaving that wakes an empty queue leaves both waiting, which is a
    /// deadlock and a finding. The fixture is used where that outcome is expected rather than
    /// where it is not.
    TwoWaiters,
    /// Branches on a byte it drew, and again on the next one.
    ///
    /// The one fixture whose *control flow* depends on the tape, which is what a search needs to
    /// have anything to follow: every other guest here walks the same path whatever it draws, so
    /// its coverage is the same on every seed and tells a corpus nothing. Two nested branches make
    /// four paths, one of which is reached by a quarter of the seeds — enough for a test to prove
    /// that coverage responds to the tape without needing a campaign to find it.
    CoinFlips,
    /// The same, eight times over: two hundred and fifty-six paths.
    ///
    /// What a campaign needs and four paths cannot give it. With four, a search exhausts the guest
    /// in its first generation and every later one is a formality — which makes a test of "does
    /// the order results arrive in matter" pass because nothing was left to decide. Two hundred
    /// and fifty-six paths keep the corpus growing long enough for the question to have an answer.
    ManyFlips,
}

impl Fixture {
    /// Every fixture that completes under any interleaving.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::ClockAndRandom, Self::TwoTasks, Self::LargeRandom]
    }

    /// Short name, as it appears on the command line and in output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ClockAndRandom => "clock-and-random",
            Self::TwoTasks => "two-tasks",
            Self::LargeRandom => "large-random",
            Self::TwoWaiters => "two-waiters",
            Self::CoinFlips => "coin-flips",
            Self::ManyFlips => "many-flips",
        }
    }

    /// The operations this guest performs.
    #[must_use]
    pub fn program(self) -> NullProgram {
        match self {
            Self::ClockAndRandom => NullProgram::single(vec![
                GuestOp::Syscall { nr: sys::CLOCK_GETTIME, args: [0, MEMORY_BASE, 0, 0, 0, 0] },
                GuestOp::Syscall { nr: sys::GETRANDOM, args: [MEMORY_BASE + 64, 32, 0, 0, 0, 0] },
                GuestOp::Syscall { nr: sys::WRITE, args: [1, MEMORY_BASE + 64, 32, 0, 0, 0] },
                GuestOp::Syscall {
                    nr: sys::CLOCK_GETTIME,
                    args: [0, MEMORY_BASE + 128, 0, 0, 0, 0],
                },
                GuestOp::Exit(0),
            ]),

            Self::TwoTasks => NullProgram {
                main: vec![
                    GuestOp::Spawn,
                    GuestOp::Syscall { nr: sys::GETRANDOM, args: [MEMORY_BASE, 16, 0, 0, 0, 0] },
                    GuestOp::Syscall {
                        nr: sys::CLOCK_GETTIME,
                        args: [0, MEMORY_BASE + 64, 0, 0, 0, 0],
                    },
                    GuestOp::Syscall { nr: sys::READ, args: [0, MEMORY_BASE + 96, 8, 0, 0, 0] },
                    GuestOp::Exit(0),
                ],
                children: vec![vec![
                    GuestOp::Syscall {
                        nr: sys::GETRANDOM,
                        args: [MEMORY_BASE + 128, 16, 0, 0, 0, 0],
                    },
                    GuestOp::Syscall {
                        nr: sys::CLOCK_GETTIME,
                        args: [0, MEMORY_BASE + 192, 0, 0, 0, 0],
                    },
                    GuestOp::Exit(0),
                ]],
            },

            Self::LargeRandom => NullProgram::single(vec![
                GuestOp::Syscall { nr: sys::GETRANDOM, args: [MEMORY_BASE, 4096, 0, 0, 0, 0] },
                GuestOp::Exit(0),
            ]),

            Self::TwoWaiters => {
                // The word starts at zero and nothing writes it, so both waits take hold and the
                // single wake has two candidates to choose between.
                let wait = GuestOp::Syscall {
                    nr: sys::FUTEX,
                    args: [MEMORY_BASE, futex_op::WAIT, 0, 0, 0, 0],
                };
                NullProgram {
                    main: vec![
                        GuestOp::Spawn,
                        GuestOp::Spawn,
                        GuestOp::Syscall {
                            nr: sys::FUTEX,
                            args: [MEMORY_BASE, futex_op::WAKE, 2, 0, 0, 0],
                        },
                        GuestOp::Exit(0),
                    ],
                    children: vec![
                        vec![wait.clone(), GuestOp::Exit(0)],
                        vec![wait, GuestOp::Exit(0)],
                    ],
                }
            }

            Self::ManyFlips => {
                const FLIPS: usize = 8;
                let mut ops = Vec::new();
                for i in 0..FLIPS {
                    ops.push(GuestOp::Syscall {
                        nr: sys::GETRANDOM,
                        args: [MEMORY_BASE + (i as u64) * 8, 1, 0, 0, 0, 0],
                    });
                }
                // Each flip skips one of two calls, so the paths differ in which operations ran
                // rather than only in which order they ran. Coverage is about the former.
                for i in 0..FLIPS {
                    let base = FLIPS + i * 4;
                    ops.push(GuestOp::JumpIfBit {
                        addr: GuestAddr(MEMORY_BASE + (i as u64) * 8),
                        bit: 0,
                        pc: base + 3,
                    });
                    ops.push(GuestOp::Syscall { nr: sys::GETPID, args: [0; 6] });
                    ops.push(GuestOp::Jump { pc: base + 4 });
                    ops.push(GuestOp::Syscall { nr: sys::GETTID, args: [0; 6] });
                }
                ops.push(GuestOp::Exit(0));
                NullProgram::single(ops)
            }

            Self::CoinFlips => {
                // One drawn byte per branch, each read down to its lowest bit: a whole-word
                // comparison would branch once in two hundred and fifty-six runs, which is a
                // coin nobody can flip. The two bytes are far enough apart to stay independent.
                let draw =
                    |at: u64| GuestOp::Syscall { nr: sys::GETRANDOM, args: [at, 1, 0, 0, 0, 0] };
                NullProgram::single(vec![
                    draw(MEMORY_BASE),
                    draw(MEMORY_BASE + 8),
                    // Each branch skips a distinct operation, so the four paths walk four
                    // different edge sets rather than four orderings of one.
                    GuestOp::JumpIfBit { addr: GuestAddr(MEMORY_BASE), bit: 0, pc: 5 },
                    GuestOp::Syscall {
                        nr: sys::CLOCK_GETTIME,
                        args: [0, MEMORY_BASE + 64, 0, 0, 0, 0],
                    },
                    GuestOp::Jump { pc: 6 },
                    GuestOp::Syscall { nr: sys::GETPID, args: [0; 6] },
                    GuestOp::JumpIfBit { addr: GuestAddr(MEMORY_BASE + 8), bit: 0, pc: 9 },
                    GuestOp::Syscall { nr: sys::GETTID, args: [0; 6] },
                    GuestOp::Jump { pc: 10 },
                    GuestOp::Syscall { nr: sys::SCHED_YIELD, args: [0; 6] },
                    GuestOp::Exit(0),
                ])
            }
        }
    }
}

/// Runs one scripted program under the lone world, and hands back the backend with the report.
///
/// **The one place a single-guest run is assembled**, so a change to the shape of a run is made
/// once -- rule 7's own hazard is several helpers building the same `RunInputs` and one of them
/// being missed.
fn drive_one(fixture: Fixture, tape: &Tape) -> (RunReport, NullBackend) {
    let mut backend = NullBackend::new(fixture.program());
    let mut world = MinimalWorld::new();
    let mut policy = RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());

    let report = run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: std::slice::from_ref(&GuestSpec::default()),
        config: RunConfig::default(),
    })
    .expect("a scripted guest always completes");
    (report, backend)
}

/// Runs one scripted program under the lone world, with memory preloaded at the given addresses.
///
/// # Panics
///
/// Panics if the run cannot be carried out at all, which in a scripted guest means the harness
/// itself is broken rather than the guest.
#[must_use]
pub fn run_alone(program: NullProgram, preloads: &[(u64, Vec<u8>)], seed: Seed) -> RunReport {
    let mut backend = NullBackend::new(program);
    for (addr, bytes) in preloads {
        backend
            .preload_on(NodeId::ZERO, GuestAddr(*addr), bytes)
            .expect("preload targets the scripted node");
    }
    let mut world = MinimalWorld::new();
    let mut policy = RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());
    run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape: &Tape::from_seed(seed),
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: std::slice::from_ref(&GuestSpec::default()),
        config: RunConfig::default(),
    })
    .expect("a scripted guest always completes")
}

/// Runs a fixture once under `seed` and returns what the kernel reported.
///
/// # Panics
///
/// Panics if the run cannot be carried out at all, which in a scripted guest means the harness
/// itself is broken rather than the guest.
#[must_use]
pub fn run_once(fixture: Fixture, seed: Seed) -> RunReport {
    drive_one(fixture, &Tape::from_seed(seed)).0
}

/// Runs a fixture once and returns what it covered as well as what it reported.
///
/// The pair is what a search works from: the report says whether the run is a finding, and the map
/// says whether it went anywhere new. Both come from the same run, because asking twice would be
/// asking about two runs.
///
/// # Panics
///
/// Panics if the run cannot be carried out at all.
#[must_use]
pub fn run_once_covered(fixture: Fixture, seed: Seed) -> (RunReport, CoverageMap) {
    let (report, mut backend) = drive_one(fixture, &Tape::from_seed(seed));
    let covered = backend.coverage().cloned().unwrap_or_default();
    (report, covered)
}

/// Carries out one piece of a campaign's work: runs the tape it names and reports what happened.
///
/// The bridge between the search and a guest, in the form the search asks for. Everything the
/// explorer is entitled to know about a run is here and nothing else is, which is what keeps a
/// campaign a function of its results rather than of the machinery underneath them.
///
/// # Panics
///
/// Panics if the run cannot be carried out at all.
#[must_use]
pub fn carry_out(fixture: Fixture, item: &WorkItem) -> RunResult {
    let tape = Tape::with_prefix(item.seed, item.prefix.clone());
    let (report, mut backend) = drive_one(fixture, &tape);

    RunResult {
        index: item.index,
        digest: report.digest,
        // The scripted fixtures never violate anything, so a finding is invented from the run's
        // own digest: what the campaign is being tested on is how it treats findings, not whether
        // this guest has any.
        outcome: (report.digest.as_bytes()[0] % 32 == 0).then(|| "guest crashed".to_owned()),
        coverage: backend.coverage().cloned().unwrap_or_default(),
        guest_edges: backend.coverage_edges(),
        journal: report.journal,
        adversity: 0,
        observed: Vec::new(),
    }
}

/// Digests of `times` runs of one fixture under one seed.
#[must_use]
pub fn digests(fixture: Fixture, seed: Seed, times: usize) -> Vec<Digest> {
    (0..times).map(|_| run_once(fixture, seed).digest).collect()
}

/// Bytes preloaded into one node's memory before the cluster starts.
pub type Preload = (u32, u64, Vec<u8>);

/// Runs scripted programs, one per node, under the distributed world.
///
/// Returns the report together with the decoded event log, because most cluster assertions are
/// about which node an event landed on.
///
/// # Panics
///
/// Panics if the run cannot be carried out at all, which in a scripted guest means the harness
/// itself is broken rather than the guest.
#[must_use]
pub fn run_cluster(
    programs: Vec<NullProgram>,
    preloads: &[Preload],
    seed: Seed,
) -> (RunReport, Vec<Event>) {
    run_scenario(programs, preloads, Scenario::default(), seed)
}

/// Names the world model cluster runs are driven by.
#[must_use]
pub fn distributed_identity() -> bohr_types::WorldModelId {
    WorldModel::identity(&DistributedWorld::new())
}

/// [`run_cluster`], with the network wired to the given configuration.
///
/// # Panics
///
/// Panics if the run cannot be carried out at all, which in a scripted guest means the harness
/// itself is broken rather than the guest.
#[must_use]
pub fn run_networked(
    programs: Vec<NullProgram>,
    preloads: &[Preload],
    config: bohr_world::NetworkConfig,
    seed: Seed,
) -> (RunReport, Vec<Event>) {
    run_scenario(programs, preloads, Scenario { network: config, ..Scenario::default() }, seed)
}

/// Everything a scripted cluster scenario can configure.
#[derive(Debug, Default)]
pub struct Scenario {
    /// The virtual network's wiring and schedule.
    pub network: bohr_world::NetworkConfig,
    /// The virtual disk's failure modes.
    pub disk: bohr_world::DiskConfig,
    /// Node faults on the scenario clock.
    pub node_faults: Vec<bohr_world::ScheduledNodeFault>,
    /// How drawn node faults choose their moment, where a scenario arms any.
    pub fault_policy: bohr_world::FaultPolicy,
    /// The path a guest is told to write its records to, where this run offers one.
    pub sdk_output: Option<String>,
    /// How long a healed cluster may commit nothing, where the scenario watches for it.
    pub liveness: Option<bohr_world::LivenessConfig>,
    /// Whether an answer given before a sync and lost to a crash is a finding.
    pub durability: Option<bohr_world::DurabilityConfig>,
}

/// [`run_cluster`], with the whole scenario wired: network, disk and node faults.
///
/// # Panics
///
/// Panics if the run cannot be carried out at all, which in a scripted guest means the harness
/// itself is broken rather than the guest.
#[must_use]
pub fn run_scenario(
    programs: Vec<NullProgram>,
    preloads: &[Preload],
    scenario: Scenario,
    seed: Seed,
) -> (RunReport, Vec<Event>) {
    refusable_scenario(programs, preloads, scenario, seed)
        .expect("a scripted cluster always completes")
}

/// The same, answering a refusal rather than panicking on it.
///
/// What a rule about this world's own defects has to be tested through. Those rules stop a run
/// instead of reporting it — a guest shown something no machine could show it is not evidence
/// about the guest — and the whole claim is the refusal, which a harness that panicked on it
/// could not read.
///
/// # Errors
///
/// The refusal, where the run made one.
///
/// # Panics
///
/// Panics if the scripted cluster itself is malformed — a preload aimed at a node with no program,
/// or a log the sink cannot close — which is the harness being broken rather than the run.
pub fn refusable_scenario(
    programs: Vec<NullProgram>,
    preloads: &[Preload],
    scenario: Scenario,
    seed: Seed,
) -> Result<(RunReport, Vec<Event>), KernelError> {
    let tape = Tape::from_seed(seed);
    let node_count = programs.len();
    let mut backend = NullBackend::cluster(programs);
    for (node, addr, bytes) in preloads {
        backend
            .preload_on(NodeId(*node), GuestAddr(*addr), bytes)
            .expect("preload targets a scripted node");
    }
    let mut world = DistributedWorld::new()
        .with_network(scenario.network)
        .with_disk(scenario.disk)
        .with_node_faults(scenario.node_faults)
        .with_fault_policy(scenario.fault_policy);
    if let Some(liveness) = scenario.liveness {
        world = world.with_liveness(liveness);
    }
    if let Some(durability) = scenario.durability {
        world = world.with_durability(durability);
    }
    if let Some(path) = scenario.sdk_output {
        world = world.with_sdk_output(path);
    }
    let mut policy = RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());
    let specs = vec![GuestSpec::default(); node_count];

    let report = run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape: &tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: &specs,
        config: RunConfig::default(),
    })?;

    let (log, digest, _) = sink.finish().expect("the sink closes");
    assert_eq!(digest, report.digest, "the sink and the report must agree on what ran");
    let events = read_events(&log).expect("the log decodes");
    Ok((report, events))
}

/// Everything a run is entitled to be judged on.
///
/// Wider than [`run_scenario`] returns, because a verdict is not read from the timeline alone: the
/// records the guests wrote about themselves say which promise was broken and where it is written,
/// and nothing else in the run carries that.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Judged {
    /// What the kernel reported, including the outcome the real oracle reached.
    pub report: RunReport,
    /// The decoded event log.
    pub events: Vec<Event>,
    /// What the guests wrote about themselves, one record per line.
    pub sdk_records: Vec<u8>,
}

/// [`run_scenario`], judged by the oracle the tool ships and driven by a tape the caller owns.
///
/// The determinism harness runs an oracle with no opinion on purpose: a determinism test measures
/// reproduction, and a verdict would only get in the way of comparing two runs. Reduction asks the
/// opposite question — the verdict *is* what is under test — so this runs the real one.
///
/// The tape is passed rather than a seed because a reduction runs edited tapes: the whole point is
/// to try the same scenario with one decision changed.
///
/// # Panics
///
/// Panics if the run cannot be carried out at all, which in a scripted guest means the harness
/// itself is broken rather than the guest.
#[must_use]
pub fn judge_scenario(
    programs: Vec<NullProgram>,
    preloads: &[Preload],
    scenario: Scenario,
    tape: &Tape,
    config: RunConfig,
) -> Judged {
    let node_count = programs.len();
    let mut backend = NullBackend::cluster(programs);
    for (node, addr, bytes) in preloads {
        backend
            .preload_on(NodeId(*node), GuestAddr(*addr), bytes)
            .expect("preload targets a scripted node");
    }
    let mut world = DistributedWorld::new()
        .with_network(scenario.network)
        .with_disk(scenario.disk)
        .with_node_faults(scenario.node_faults)
        .with_fault_policy(scenario.fault_policy);
    let watching = bohr_world::Watching {
        liveness: scenario.liveness.is_some(),
        durability: scenario.durability.is_some(),
        probed: bohr_world::Probed::default(),
    };
    if let Some(liveness) = scenario.liveness {
        world = world.with_liveness(liveness);
    }
    if let Some(durability) = scenario.durability {
        world = world.with_durability(durability);
    }
    if let Some(path) = scenario.sdk_output {
        world = world.with_sdk_output(path);
    }
    let mut policy = RandomPolicy;
    // The panel the tool would wire for this scenario, not a fixed oracle: a fixture that judged
    // with less than the tool does would prove something about the fixture.
    let mut oracle = bohr_world::panel(watching);
    let mut sink = TraceWriter::new(Vec::new());
    let specs = vec![GuestSpec::default(); node_count];

    let report = run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: &specs,
        config,
    })
    .expect("a scripted cluster always completes");

    // The world's borrow ended with the run, so what it collected can be read now.
    let sdk_records = world.sdk_records();
    let (log, digest, _) = sink.finish().expect("the sink closes");
    assert_eq!(digest, report.digest, "the sink and the report must agree on what ran");
    let events = read_events(&log).expect("the log decodes");
    Judged { report, events, sdk_records }
}

/// The `sockaddr_in` bytes naming a node and port, for preloading into a scripted guest.
#[must_use]
pub fn sockaddr_for(node: u32, port: u16) -> Vec<u8> {
    let mut out = vec![0u8; 16];
    out[0..2].copy_from_slice(&2u16.to_le_bytes());
    out[2..4].copy_from_slice(&port.to_be_bytes());
    out[4..8].copy_from_slice(&(0x0A42_0000u32 | (node + 1)).to_be_bytes());
    out
}

/// An oracle that judges nothing, so a determinism test measures reproduction and not verdicts.
#[derive(Debug)]
struct AcceptEverything;

impl bohr_kernel::Oracle for AcceptEverything {
    fn identity(&self) -> bohr_types::OracleId {
        bohr_types::OracleId::new("accept-everything", 1)
    }

    fn judge(&mut self, _outcome: &bohr_kernel::RunOutcome) -> bohr_kernel::Verdict {
        bohr_kernel::Verdict::Clean
    }
}

/// Names the world model a fixture run is driven by, so a report can say what produced it.
#[must_use]
pub fn world_identity() -> bohr_types::WorldModelId {
    WorldModel::identity(&MinimalWorld::new())
}
