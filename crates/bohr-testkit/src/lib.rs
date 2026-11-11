//! Fixtures and the harness the determinism suite is built on.
//!
//! Scripted guests run in process on any system without privileges, so the clock, scheduler, world
//! and trace are tested without interception. `bohr selftest` runs the same guests.

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
    /// The tape decides which waiter goes first.
    ///
    /// Absent from [`Fixture::all`]: an interleaving that wakes before both wait deadlocks.
    TwoWaiters,
    /// Branches on a byte it drew, and again on the next one.
    ///
    /// Its control flow depends on the tape, giving four paths, so coverage varies by seed.
    CoinFlips,
    /// The same, eight times over: two hundred and fifty-six paths.
    ///
    /// Enough paths that a campaign keeps growing its corpus past the first generation.
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
                // The word stays zero, so both waits take hold.
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
                // Each flip skips a call, so paths differ in which operations ran.
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
                // One drawn byte per branch, tested on its lowest bit.
                let draw =
                    |at: u64| GuestOp::Syscall { nr: sys::GETRANDOM, args: [at, 1, 0, 0, 0, 0] };
                NullProgram::single(vec![
                    draw(MEMORY_BASE),
                    draw(MEMORY_BASE + 8),
                    // Each branch skips a distinct operation.
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
/// The one place a single-guest run is assembled (rule 7).
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
/// Panics if the harness is broken.
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
/// Panics if the harness is broken.
#[must_use]
pub fn run_once(fixture: Fixture, seed: Seed) -> RunReport {
    drive_one(fixture, &Tape::from_seed(seed)).0
}

/// Runs a fixture once and returns what it covered as well as what it reported.
///
/// Both come from one run: the report gives the finding and the map gives novelty.
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
/// Returns exactly what the explorer may know about a run.
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
        // Fixtures never fail, so a finding is derived from the digest to exercise the campaign.
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
/// Returns the report and the decoded event log.
///
/// # Panics
///
/// Panics if the harness is broken.
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
/// Panics if the harness is broken.
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
    /// The path each node's program is started from, which `/proc/self/exe` answers; none where
    /// empty.
    pub programs: Vec<String>,
}

/// [`run_cluster`], with the whole scenario wired: network, disk and node faults.
///
/// # Panics
///
/// Panics if the harness is broken.
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

/// The same, returning a refusal as an error.
///
/// For testing rules that refuse a run, where the refusal is the result.
///
/// # Errors
///
/// The refusal, where the run made one.
///
/// # Panics
///
/// Panics if the scripted cluster is malformed.
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
        .with_fault_policy(scenario.fault_policy)
        .with_programs(scenario.programs);
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
/// Adds the guests' records, which say which promise broke and where.
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
/// Uses the real oracle panel, unlike the determinism harness. Takes a tape, since reduction runs
/// edited tapes.
///
/// # Panics
///
/// Panics if the harness is broken.
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
        .with_fault_policy(scenario.fault_policy)
        .with_programs(scenario.programs);
    let watching = bohr_world::Watching {
        liveness: scenario.liveness.is_some(),
        durability: scenario.durability.is_some(),
        laws: false,
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
    // The panel the tool wires for this scenario.
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

    // The run has released the world.
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
