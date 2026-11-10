//! The distributed-systems world: one model, many machines.
//!
//! Each node has its own world: files, descriptors and futex space. Nodes share the timer queue.
//! Sleeps and timed waits park the task and arm a timer, so one node's sleep never advances the
//! others' clocks.

mod disk;
mod epoll;
pub(crate) mod net;
mod sdk;
mod timers;
mod watch;

use std::collections::{BTreeMap, BTreeSet};

use bohr_backend::{Resume, Stop, StopKind};
use bohr_kernel::{
    DueEffects, Effects, FaultCatalog, KernelError, MAX_DRIFT_PPM, NodeOp, TaskState, Wake,
    WorldCtx, WorldModel,
};
use bohr_tape::DrawSite;
use bohr_trace::{DiskOpKind, Endpoint, EventKind, FaultKind, FaultTarget, NodeState};
use bohr_types::{Fd, GuestAddr, NodeId, TaskId, Tick, WorldModelId};
pub use disk::DiskConfig;
use disk::NodeDisk;
use epoll::Epolls;
pub use net::{GreyWindow, NetworkConfig, ScheduledPartition, Topology};
use net::{Network, PendingAcceptArgs, PendingRecvArgs, decode_sockaddr};
use timers::{Due, TimerQueue};
use watch::{Durability, Liveness};
pub use watch::{DurabilityConfig, LivenessConfig};

use crate::{
    minimal::{MinimalWorld, SYSCALL_COST, TIMER_ABSTIME, futex_op},
    nums::{errno, sys},
    ring,
};

/// The `poll` event bits this world answers.
const POLLIN: u16 = 0x001;
const POLLOUT: u16 = 0x004;
const POLLNVAL: u16 = 0x020;
const POLLRDNORM: u16 = 0x040;
const POLLWRNORM: u16 = 0x100;
/// The most entries one `poll` may name, as the minimal world bounds it.
const MAX_POLL_ENTRIES: usize = 1024;

/// How a drawn node fault picks the moment it fires.
///
/// `docs/API.md` §5.4 claims aimed faults find ten times more; the measurement is in
/// `benches/baseline.toml`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Targeting {
    /// Any moment is as good as any other.
    #[default]
    Uniform,
    /// Only moments where a fault would cost the guest something.
    ///
    /// For the disk: the node has written since its last `fsync`.
    Guided,
}

/// One rung of how bad a drawn degradation is.
struct Degradation {
    /// What the link's drop probability becomes while it is bad.
    drop: Option<(u32, u32)>,
    /// What its mean latency is multiplied by.
    latency: Option<u64>,
}

/// The rungs a drawn degradation is chosen from.
///
/// Three classes: slow but whole, lossy but prompt, and both.
const DEGRADATIONS: [Degradation; 3] = [
    Degradation { drop: None, latency: Some(32) },
    Degradation { drop: Some((1, 4)), latency: None },
    Degradation { drop: Some((1, 2)), latency: Some(8) },
];

/// Which node faults a campaign may draw.
///
/// Only faults for a running node, since a fault is drawn at a syscall. Recovery is the second
/// half of the fault.
///
/// Fixed flags, so the policy is `Copy` and the index order belongs to this type.
///
/// A one-kind set costs no tape (`docs/API.md` §2.4), so kill-only campaigns replay unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DrawnFaults {
    /// Power off: every task dies and everything unsynced is lost. Comes back as a restart.
    pub kill: bool,
    /// Pause without loss, the way a garbage-collection pause looks from outside. Comes back as a
    /// thaw.
    pub freeze: bool,
    /// Cut every link between this node and the rest, and heal them together.
    ///
    /// The whole node, so nothing beyond the fire is drawn.
    pub isolate: bool,
    /// Degrade every link of a node without cutting one, and let them recover together.
    ///
    /// The grey failure most clusters handle worst, as a drawn fault.
    pub degrade: bool,
    /// Whether a killed node comes back on the sync before its last one.
    ///
    /// A recovery choice for the kill. One step only, since a drawn depth would make the site's
    /// width depend on its value; deeper rewinds are scheduled.
    pub recover_from_snapshot: bool,
    /// Step a node's wall clock by a drawn amount, within a minute either way.
    pub clock_step: bool,
    /// Set a node's clock drifting at a drawn rate, within ±[`MAX_DRIFT_PPM`].
    pub clock_drift: bool,
}

impl Default for DrawnFaults {
    /// The kill alone, recovering on the crashed disk; the default tape layout.
    fn default() -> Self {
        Self {
            kill: true,
            freeze: false,
            isolate: false,
            degrade: false,
            recover_from_snapshot: false,
            clock_step: false,
            clock_drift: false,
        }
    }
}

impl DrawnFaults {
    /// The kinds enabled, in this type's own order.
    fn enabled(self) -> Vec<NodeFaultKind> {
        let mut kinds = Vec::with_capacity(2);
        if self.kill {
            kinds.push(NodeFaultKind::Kill);
        }
        if self.freeze {
            kinds.push(NodeFaultKind::Freeze);
        }
        if self.isolate {
            // A kind for the draw, but applied at the fire; see `maybe_kill`.
            kinds.push(NodeFaultKind::Isolate);
        }
        if self.degrade {
            kinds.push(NodeFaultKind::Degrade);
        }
        // Size zero; drawn after the kind, so other scenarios pay nothing.
        if self.clock_step {
            kinds.push(NodeFaultKind::ClockStep { by_ticks: 0 });
        }
        if self.clock_drift {
            kinds.push(NodeFaultKind::ClockDrift { ppm: 0 });
        }
        kinds
    }

    /// How a node comes back from `kind`, and after what a scenario said.
    const fn recovery(self, kind: &NodeFaultKind) -> Option<NodeFaultKind> {
        match kind {
            NodeFaultKind::Kill if self.recover_from_snapshot => {
                Some(NodeFaultKind::RestartFromSnapshot { back: 1 })
            }
            NodeFaultKind::Kill => Some(NodeFaultKind::Restart),
            NodeFaultKind::Freeze => Some(NodeFaultKind::Thaw),
            _ => None,
        }
    }
}

/// Whether drawn node faults are armed, how likely they are, and how they choose their moment.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FaultPolicy {
    /// Odds a fault fires at a moment it may fire at, as a rational.
    pub kill: (u32, u32),
    /// Odds an allocation call — `brk`, an anonymous `mmap`, `mremap` — is refused, as a
    /// rational; nought where memory never runs out.
    pub enomem: (u32, u32),
    /// How the moment is chosen.
    pub targeting: Targeting,
    /// Ticks after a drawn kill before the node is started again. Zero leaves it dead.
    ///
    /// Recovery tests whether what the node acknowledged is still there.
    pub restart_after: Tick,
    /// Virtual time before which a drawn fault is not offered at all.
    ///
    /// An execution client makes 130 000 calls starting up, so even the finest odds kill it before
    /// it is up. Zero by default.
    pub arm_after: Tick,
    /// Virtual time from which no drawn fault fires, of any kind: node, disk, memory or network.
    ///
    /// Recoveries already armed still happen, so a system is judged on coming back once nothing
    /// pushes it down. `None` keeps the window open to the end.
    pub disarm_after: Option<Tick>,
    /// How many drawn faults one run may fire, where a scenario puts a ceiling on it.
    ///
    /// Odds set when; this sets how many. 270 kills in 20 runs of an execution client tested only
    /// restarts. A spent budget returns zero odds and still draws.
    pub budget_per_run: Option<u32>,
    /// Which kinds a fire may be, chosen by a draw over this set.
    pub kinds: DrawnFaults,
    /// Kinds that fire at nought odds, for an ablation: every draw still happens, so the rest of
    /// the run keeps its schedule, and only these kinds' effects are left out.
    pub suppressed: Suppressed,
}

/// A set of fault kinds, as a mask over `FaultKind`'s declaration order.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Suppressed(u32);

impl Suppressed {
    /// No kind suppressed.
    pub const NONE: Self = Self(0);

    /// The same set, with `kind` in it.
    #[must_use]
    pub const fn with(self, kind: FaultKind) -> Self {
        Self(self.0 | (1 << kind as u32))
    }

    /// Whether `kind` is in the set.
    #[must_use]
    pub const fn contains(self, kind: FaultKind) -> bool {
        self.0 & (1 << kind as u32) != 0
    }

    /// `odds`, or nought where `kind` is in the set.
    pub(crate) const fn odds(self, kind: FaultKind, odds: (u32, u32)) -> (u32, u32) {
        if self.contains(kind) { (0, 1) } else { odds }
    }
}

/// The fault kind a drawn node fault lands as, which ablation names it by.
const fn landed_as(kind: &NodeFaultKind) -> FaultKind {
    match kind {
        NodeFaultKind::Kill => FaultKind::NodeKill,
        NodeFaultKind::Freeze | NodeFaultKind::Thaw => FaultKind::NodeFreeze,
        NodeFaultKind::Isolate => FaultKind::NetPartition,
        NodeFaultKind::Degrade => FaultKind::NetDrop,
        NodeFaultKind::Restart | NodeFaultKind::RestartFromSnapshot { .. } => {
            FaultKind::NodeRestart
        }
        NodeFaultKind::ChainReorg { .. } => FaultKind::ChainReorg,
        NodeFaultKind::ClockStep { .. } => FaultKind::ClockStep,
        NodeFaultKind::ClockDrift { .. } => FaultKind::ClockDrift,
    }
}

impl FaultPolicy {
    /// The odds at this moment, given whether it is one the targeting cares about.
    ///
    /// Always odds, never a skipped draw, so consumption depends only on the request. Zero before
    /// the policy is armed.
    ///
    /// Measured on the world clock, so a scenario's moment is the same on every node.
    const fn odds_at(self, candidate: bool, now: Tick, fired: u32) -> (u32, u32) {
        if now.get() < self.arm_after.get() {
            return (0, 1);
        }
        if !self.open_at(now) {
            return (0, 1);
        }
        if let Some(budget) = self.budget_per_run
            && fired >= budget
        {
            return (0, 1);
        }
        match self.targeting {
            Targeting::Uniform => self.kill,
            Targeting::Guided if candidate => self.kill,
            Targeting::Guided => (0, 1),
        }
    }

    /// Whether drawn faults may still fire at `now`.
    pub(crate) const fn open_at(self, now: Tick) -> bool {
        match self.disarm_after {
            Some(closes) => now.get() < closes.get(),
            None => true,
        }
    }

    /// `odds` at `now`, nought once the window has closed. The draw happens either way.
    pub(crate) const fn windowed(self, now: Tick, odds: (u32, u32)) -> (u32, u32) {
        if self.open_at(now) { odds } else { (0, 1) }
    }

    /// Whether this policy does anything at all.
    const fn is_armed(self) -> bool {
        self.kill.0 > 0 && self.kill.1 > 0
    }
}

/// A node fault fired by the scenario clock.
///
/// The scheduled form makes a hand-written scenario a regression test.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScheduledNodeFault {
    /// When the fault fires.
    pub at_tick: Tick,
    /// The node it targets.
    pub node: NodeId,
    /// What happens to it.
    pub kind: NodeFaultKind,
}

/// What a scheduled node fault does.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NodeFaultKind {
    /// Power off: every task dies, everything unsynced is lost.
    Kill,
    /// Pause without loss: deliveries and wakes accumulate and land at the thaw, the way a
    /// garbage-collection pause looks from outside.
    Freeze,
    /// Cut every link between this node and the rest of the cluster, and heal them together.
    ///
    /// Drawn only; scheduled cuts use `[[partition]]`.
    Isolate,
    /// Make every link of this node bad without breaking it, and let it recover.
    ///
    /// Drawn only, like [`Self::Isolate`]; severity from `DEGRADATIONS`.
    Degrade,
    /// Let a frozen node run again.
    Thaw,
    /// Start a dead node again from its spec, with the disk its crash left.
    Restart,
    /// Start a dead node again, its disk reset to an older synced image: `back` counts snapshots
    /// from the newest, so `0` is the latest `fsync` and `1` the one before it.
    RestartFromSnapshot {
        /// Snapshots to step back from the newest.
        back: usize,
    },
    /// Rewind a set of nodes at once to a common older image, and let history run again.
    ///
    /// Protocol-free: every node in the set returns to an older synced image at one moment and
    /// later history is discarded.
    ChainReorg {
        /// Snapshots to step back from each node's newest.
        back: usize,
        /// The nodes rewound, in order. Empty means every node the cluster has.
        nodes: Vec<NodeId>,
    },
    /// Step a node's wall clock by `by_ticks`, back where negative, as `settimeofday` does. Its
    /// monotonic clock never moves.
    ClockStep {
        /// How far the wall clock moves, in ticks.
        by_ticks: i64,
    },
    /// Make a node's clock run `ppm` parts per million fast from now on, slow where negative.
    /// Readings before the change stay what they were.
    ClockDrift {
        /// The new rate, within ±[`MAX_DRIFT_PPM`].
        ppm: i32,
    },
}

/// A minute either way, covering NTP corrections and second-scale leases.
const MAX_DRAWN_CLOCK_STEP: u64 = 60 * Tick::SECOND.get();

/// Socket-family syscall numbers, kept here because only this model answers them.
mod socket_sys {
    /// `socket`.
    pub(crate) const SOCKET: u32 = 41;
    /// `connect`.
    pub(crate) const CONNECT: u32 = 42;
    /// `sendto`.
    pub(crate) const SENDTO: u32 = 44;
    /// `recvfrom`.
    pub(crate) const RECVFROM: u32 = 45;
    /// `shutdown`.
    pub(crate) const SHUTDOWN: u32 = 48;
    /// `bind`.
    pub(crate) const BIND: u32 = 49;
    /// `getsockname`.
    pub(crate) const GETSOCKNAME: u32 = 51;
    /// `socketpair`.
    pub(crate) const SOCKETPAIR: u32 = 53;
    /// `setsockopt`.
    pub(crate) const SETSOCKOPT: u32 = 54;
    /// `getsockopt`.
    pub(crate) const GETSOCKOPT: u32 = 55;
    /// `sendmsg`.
    pub(crate) const SENDMSG: u32 = 46;
    /// `recvmsg`.
    pub(crate) const RECVMSG: u32 = 47;
    /// `accept`.
    pub(crate) const ACCEPT: u32 = 43;
    /// `listen`.
    pub(crate) const LISTEN: u32 = 50;
    /// `getpeername`.
    pub(crate) const GETPEERNAME: u32 = 52;
    /// `accept4`: the same as `accept`, with the flags the descriptor is born with.
    pub(crate) const ACCEPT4: u32 = 288;

    /// The kind of socket `socket` was asked for, once the flags beside it are masked off.
    pub(crate) const SOCK_KIND: u64 = 0xFF;
    /// A byte stream.
    pub(crate) const SOCK_STREAM: u64 = 1;
    /// The descriptor is born non-blocking, which is how every event loop opens one.
    pub(crate) const SOCK_NONBLOCK: u64 = 0o4000;
}

/// The readiness family, kept beside the sockets it mostly reports on.
mod poll_sys {
    /// `epoll_wait`.
    pub(crate) const EPOLL_WAIT: u32 = 232;
    /// `epoll_ctl`.
    pub(crate) const EPOLL_CTL: u32 = 233;
    /// `epoll_pwait`: the same, with a signal mask this world has nothing to do with.
    pub(crate) const EPOLL_PWAIT: u32 = 281;
    /// `eventfd2`.
    pub(crate) const EVENTFD2: u32 = 290;
    /// `epoll_create1`.
    pub(crate) const EPOLL_CREATE1: u32 = 291;
}

/// A node answered somebody with writes behind it that were not yet durable.
///
/// A domain event (`EventKind::Domain`), since only this model knows acknowledgements.
const DOMAIN_ANSWERED: u32 = 1;

/// History was rewound and let run again, with how deep it was asked to go and how deep it went.
///
/// Carries the depth, which a clamped rewind may change.
const DOMAIN_REORG: u32 = 2;

/// What a node had answered for and not yet made durable at the moment a crash took it.
///
/// A measurement: it shows whether a campaign's crashes ever had data at risk.
const DOMAIN_EXPOSED: u32 = 3;

/// This model's version, recorded in every bundle and checked before a replay.
///
/// Incremented when behaviour changes stored runs. Version 2: per-file durability and per-sector
/// crashes.
const MODEL_VERSION: u16 = 3;

/// The namespace this model's domain events are read in.
///
/// Derived from the model version.
fn domain_ns() -> WorldModelId {
    WorldModelId::new("distributed", MODEL_VERSION)
}

/// `fcntl` commands a runtime asks about a descriptor it just opened.
const F_GETFD: u64 = 1;
/// Set the descriptor flags.
const F_SETFD: u64 = 2;
/// Read the file status flags.
const F_GETFL: u64 = 3;
/// Set the file status flags.
const F_SETFL: u64 = 4;

/// Duplicate a descriptor, answering with the lowest free one at or above the third argument.
const F_DUPFD: u64 = 0;
/// The same, with the copy closed on exec.
const F_DUPFD_CLOEXEC: u64 = 1030;

/// `O_RDWR`, which every descriptor this model hands out is.
const O_RDWR: i64 = 2;
/// `O_NONBLOCK`: nothing this model hands out ever blocks a caller that asked not to be blocked.
const O_NONBLOCK: i64 = 0o4000;

/// `FIONBIO`: the ioctl an event loop uses to make a socket non-blocking.
const FIONBIO: u64 = 0x5421;

/// Largest datagram the model moves in one send.
const MAX_DATAGRAM: usize = 64 * 1024;

/// The descriptor a socket call names, which is always its first argument.
fn fd_of(args: [u64; 6]) -> i64 {
    args[0] as i64
}

/// What a delegated call answered, read back off its effects for the disk's own record.
fn answered(effects: &Effects) -> i64 {
    match effects.state {
        TaskState::Runnable(Resume::Emulate { ret } | Resume::EmulateAndSignal { ret, .. }) => ret,
        _ => 0,
    }
}

/// The diagnostic code a failed per-run assertion raises, for an oracle that judges on it.
#[must_use]
pub const fn sdk_violation_code() -> &'static str {
    sdk::VIOLATION_CODE
}

/// The path a call named, as far as recognising one descriptor needs it.
///
/// Read with the node's reader, since a path near the end of its mapping cannot be read as a fixed
/// block.
fn opened_path(ctx: &mut WorldCtx<'_, '_>, addr: GuestAddr) -> Result<Option<String>, KernelError> {
    // Only the name, to recognise the opened descriptor.
    Ok(match crate::minimal::read_c_string(ctx, addr)? {
        crate::minimal::PathArg::Named(name) => Some(name),
        _ => None,
    })
}

/// True with probability `num/den`, always consuming two bytes.
///
/// Unlike [`WorldCtx::draw_bool`], certainties also cost two bytes. The journal records whether it
/// fired, for the reducer.
pub(crate) fn draw_odds(
    ctx: &mut WorldCtx<'_, '_>,
    (num, den): (u32, u32),
    site: bohr_tape::DrawSite,
) -> Result<bool, KernelError> {
    Ok(ctx.draw_odds(num, den, site))
}

/// Answers every node's syscalls from that node's own world, and owns the cluster's timers.
pub struct DistributedWorld {
    nodes: BTreeMap<NodeId, MinimalWorld>,
    /// Each task's node, for kernel callbacks without a context.
    node_of_task: BTreeMap<TaskId, NodeId>,
    timers: TimerQueue,
    /// Each task's futex waits, counted, so a timer armed for one wait cannot end a later one.
    futex_seq: BTreeMap<TaskId, u64>,
    net: Network,
    disks: BTreeMap<NodeId, NodeDisk>,
    disk_config: DiskConfig,
    node_faults: Vec<ScheduledNodeFault>,
    /// Records the guests wrote about themselves, and the descriptors they wrote them on.
    sdk: sdk::SdkRecords,
    sdk_fds: BTreeSet<(NodeId, i64)>,
    /// The path an SDK is told to write to, when this run offers one.
    sdk_path: Option<String>,
    /// Which nodes have written to their disk since they last asked a socket for something.
    ///
    /// A reply with a write behind it is what an acknowledgement looks like from outside.
    wrote_since_asking: BTreeSet<NodeId>,
    /// How a drawn node fault chooses its moment, where a campaign asked for drawn ones.
    fault_policy: FaultPolicy,
    /// How many drawn faults have fired, against the budget a scenario put on the run.
    faults_fired: u32,
    /// An assertion that failed, which makes the run a finding.
    violated: Option<String>,
    /// One sink for every node's output, in schedule order.
    output: Option<crate::OutputSink>,
    /// The end of what each incarnation of each node wrote out, for a bundle to keep.
    ///
    /// Kept only for a run whose log is kept.
    tails: Option<bohr_trace::OutputTails>,
    catalog: FaultCatalog,
    /// What every node's world is made of.
    template: NodeTemplate,
    /// Watches for a cluster that healed and went on committing nothing, where one was asked for.
    liveness: Option<Liveness>,
    /// Watches for a node that answered before it synced, where one was asked for.
    durability: Option<Durability>,
    /// Readiness sets and event counters, which span a node's files and its sockets both.
    epolls: Epolls,
    /// Judges what the probe reported, where the scenario has probes.
    observer: Option<crate::probe::Observer>,
    /// The node the probe runs on, which this world never faults and never counts as silent.
    probe_node: Option<NodeId>,
}

/// A node's world as this cluster configures one.
///
/// One function, so every path that first needs a node's world builds the same one.
fn fresh_node(node: NodeId, template: &NodeTemplate) -> MinimalWorld {
    let mut world = node_shell(template);
    if let Some(program) = template.programs.get(node.get() as usize) {
        world = world.with_program(program.clone());
    }
    if let Some(state) = template.state.as_ref() {
        world = world.from_state(state);
    }
    world
}

/// Everything a node's world is made of except the filesystem a template may carry.
fn node_shell(template: &NodeTemplate) -> MinimalWorld {
    let mut world = MinimalWorld::new();
    // The record channel's directory is created, since the guest did not choose it.
    if let Some(path) = template.sdk_path.as_ref() {
        world = world.providing(path);
    }
    if let Some(cwd) = template.cwd.as_ref() {
        world = world.with_cwd(cwd.clone());
    }
    if template.surveying {
        world = world.surveying();
    }
    if template.keep_output {
        world = world.keeping_output();
    }
    if template.io_uring {
        world = world.with_io_uring();
    }
    // Cluster timers end timed waits, so they can be parked.
    world.with_timeouts_armed()
}

/// A node's world holding a disk it is handed.
///
/// The prepared filesystem is skipped, since the restored image replaces it entirely.
fn restored_node(template: &NodeTemplate, image: crate::minimal::DiskImage) -> MinimalWorld {
    let mut world = node_shell(template);
    world.restore_disk(image);
    world
}

/// What every node's world is made of, whichever path made it.
///
/// One value, so every path builds a node's world the same way.
#[derive(Clone, Debug, Default)]
struct NodeTemplate {
    /// The filesystem every node starts with, where a scenario prepared one.
    state: Option<std::sync::Arc<crate::StateImage>>,
    /// What `getcwd` answers.
    cwd: Option<String>,
    /// Whether a call this build cannot model is answered.
    surveying: bool,
    /// Whether what a node writes out is kept for a projection to read.
    keep_output: bool,
    /// The path a guest is told to write records to, which every node needs somewhere to put.
    sdk_path: Option<String>,
    /// Whether every node is shown a kernel with `io_uring`.
    io_uring: bool,
    /// The program each node runs, in node order, for the calls that ask a node where it is.
    ///
    /// Per node, since a differential scenario runs different programs. A node past the list gets
    /// nothing.
    programs: Vec<String>,
}

impl Default for DistributedWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl DistributedWorld {
    /// The same world, with a probe on node `node` asking what `probes` name.
    ///
    /// The probe's node is outside the fault model and the silence watch.
    #[must_use]
    pub fn with_probes(mut self, probes: Vec<crate::ProbeSpec>, node: u32) -> Self {
        let mut observer = crate::probe::Observer::new(probes);
        if let Some(closes) = self.fault_policy.disarm_after {
            observer.judge_stalls_from(closes);
        }
        self.observer = Some(observer);
        self.probe_node = Some(NodeId(node));
        self.sdk.probe_node = Some(NodeId(node));
        self
    }

    /// The same world, told which program each node runs.
    #[must_use]
    pub fn with_programs(mut self, programs: Vec<String>) -> Self {
        self.template.programs = programs;
        self
    }

    /// A world with no nodes yet; each appears at its first stop.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            node_of_task: BTreeMap::new(),
            timers: TimerQueue::default(),
            futex_seq: BTreeMap::new(),
            net: Network::default(),
            disks: BTreeMap::new(),
            disk_config: DiskConfig::default(),
            node_faults: Vec::new(),
            sdk: sdk::SdkRecords::default(),
            sdk_fds: BTreeSet::new(),
            sdk_path: None,
            wrote_since_asking: BTreeSet::new(),
            fault_policy: FaultPolicy::default(),
            faults_fired: 0,
            violated: None,
            output: None,
            tails: None,
            template: NodeTemplate::default(),
            liveness: None,
            durability: None,
            epolls: Epolls::default(),
            observer: None,
            probe_node: None,
            // Exactly the faults this model emits; `scripts/lint-rules.sh` checks it against every
            // `FaultFired`.
            catalog: FaultCatalog::new(vec![
                FaultKind::NetDelay,
                FaultKind::NetDrop,
                FaultKind::NetDuplicate,
                FaultKind::NetReorder,
                FaultKind::NetPartition,
                FaultKind::DiskTear,
                FaultKind::DiskReorder,
                FaultKind::DiskStall,
                FaultKind::DiskFull,
                FaultKind::DiskError,
                FaultKind::NodeKill,
                FaultKind::NodeFreeze,
                FaultKind::NodeThaw,
                FaultKind::NodeRestart,
                FaultKind::ChainReorg,
                FaultKind::ClockStep,
                FaultKind::ClockDrift,
                FaultKind::MemoryExhausted,
                FaultKind::NetCorrupt,
            ]),
        }
    }

    /// The same world, wired to a network.
    ///
    /// Scheduled partitions are armed before anything runs.
    #[must_use]
    pub fn with_network(mut self, config: NetworkConfig) -> Self {
        for (index, partition) in config.partitions.iter().enumerate() {
            self.timers.arm(
                partition.at_tick,
                NodeId::ZERO,
                Due::PartitionChange { index, raise: true },
            );
            self.timers.arm(
                partition.heal_at_tick,
                NodeId::ZERO,
                Due::PartitionChange { index, raise: false },
            );
        }
        for (index, window) in config.grey.iter().enumerate() {
            self.timers.arm(window.at_tick, NodeId::ZERO, Due::GreyChange { index, open: true });
            self.timers.arm(
                window.until_tick,
                NodeId::ZERO,
                Due::GreyChange { index, open: false },
            );
        }
        self.net.configure(config);
        self
    }

    /// The same world, delivering what the guests write to their standard descriptors.
    #[must_use]
    pub fn with_output(mut self, sink: crate::OutputSink) -> Self {
        self.output = Some(sink);
        self
    }

    /// The same world, accepting SDK records written to `path`.
    ///
    /// A descriptor opened on `path` is the record channel; writes to it are records.
    #[must_use]
    pub fn with_sdk_output(mut self, path: String) -> Self {
        self.template.sdk_path = Some(path.clone());
        self.sdk_path = Some(path);
        self
    }

    /// The records the guests wrote, as the bundle stores them.
    #[must_use]
    pub fn sdk_records(&self) -> Vec<u8> {
        self.sdk.to_bytes()
    }

    /// The assertion that failed, where one did.
    #[must_use]
    pub fn violated(&self) -> Option<&str> {
        self.violated.as_deref()
    }

    /// The same world, with the disk's failure modes wired.
    #[must_use]
    pub fn with_disk(mut self, config: DiskConfig) -> Self {
        self.disk_config = config;
        self
    }

    /// The same world, with node faults on the scenario clock.
    #[must_use]
    pub fn with_node_faults(mut self, faults: Vec<ScheduledNodeFault>) -> Self {
        for (index, fault) in faults.iter().enumerate() {
            #[cfg(feature = "chaos-fault-moment-drift")]
            let at = crate::chaos::drift_tick(fault.at_tick);
            #[cfg(not(feature = "chaos-fault-moment-drift"))]
            let at = fault.at_tick;
            self.timers.arm(at, fault.node, Due::NodeFault { index });
        }
        self.node_faults = faults;
        self
    }

    /// The same cluster, watching for a silence after a heal.
    ///
    /// Off unless asked, since its diagnostics enter the digest.
    #[must_use]
    pub fn with_liveness(mut self, config: LivenessConfig) -> Self {
        self.liveness = Liveness::new(Some(config));
        // Armed here, so the span counts from the start and silent nodes are covered.
        if let Some(span) = self.liveness.as_ref().and_then(Liveness::quiet_span) {
            self.timers.arm(span, NodeId::ZERO, Due::QuietCheck);
        }
        self
    }

    /// The same cluster, keeping what its nodes write out so a projection can read it back.
    #[must_use]
    pub fn keeping_output(mut self) -> Self {
        self.template.keep_output = true;
        self
    }

    /// The same cluster, keeping the end of what each incarnation of each node writes out.
    #[must_use]
    pub fn keeping_tails(mut self) -> Self {
        self.tails = Some(bohr_trace::OutputTails::new());
        self
    }

    /// The end of what each incarnation of each node wrote out, where the cluster kept it.
    #[must_use]
    pub fn output_tails(&self) -> Vec<bohr_trace::OutputTail> {
        self.tails.as_ref().map(bohr_trace::OutputTails::records).unwrap_or_default()
    }

    /// Tells the liveness watch what this world just did to a node.
    ///
    /// Called beside every `NodeState`, which `scripts/lint-rules.sh` checks, so the watch never
    /// blames a node the world stopped. See [`watch::Liveness::node_became`].
    fn node_became(&mut self, node: NodeId, state: NodeState, now: Tick) {
        // A restart begins a new incarnation, whose output is kept apart from the last one's.
        if state == NodeState::Restarting
            && let Some(tails) = self.tails.as_mut()
        {
            tails.restarted(node.get(), now.get());
        }
        // The probe is outside both watches.
        if Some(node) == self.probe_node {
            return;
        }
        if let Some(watch) = self.liveness.as_mut() {
            watch.node_became(node, state, now);
        }
        if let Some(observer) = self.observer.as_mut() {
            match state {
                NodeState::Dead | NodeState::Frozen => observer.node_down(node.get()),
                NodeState::Running | NodeState::Restarting => observer.node_up(node.get(), now),
            }
        }
    }

    /// What one projection came to, read off the node the scenario named.
    ///
    /// The node is named, since a client's caller usually holds the answer.
    #[must_use]
    pub fn project(&self, spec: &crate::ProjectionSpec) -> Option<crate::Value> {
        self.nodes.get(&NodeId(spec.node))?.project(&spec.source)
    }

    /// The same cluster, watching for an answer the disk did not keep.
    ///
    /// Off unless asked, since what it records enters the digest.
    #[must_use]
    pub fn with_durability(mut self, config: DurabilityConfig) -> Self {
        self.durability = Durability::new(Some(config));
        self
    }

    /// The same cluster, surveying what it cannot model.
    ///
    /// Set before anything runs.
    #[must_use]
    pub fn surveying(mut self) -> Self {
        self.template.surveying = true;
        self
    }

    /// The same cluster, every node shown a kernel with `io_uring` (`[linux] io_uring = "ring"`).
    #[must_use]
    pub fn with_io_uring(mut self) -> Self {
        self.template.io_uring = true;
        self
    }

    /// The same cluster, every node starting from a prepared filesystem.
    #[must_use]
    pub fn from_state(mut self, image: crate::StateImage, cwd: Option<String>) -> Self {
        self.template.state = Some(std::sync::Arc::new(image));
        self.template.cwd = cwd;
        self
    }

    /// What the survey found across every node, by syscall number.
    ///
    /// Summed across nodes.
    #[must_use]
    pub fn surveyed(&self) -> Vec<(u32, u64)> {
        let mut total: BTreeMap<u32, u64> = BTreeMap::new();
        for world in self.nodes.values() {
            for (nr, count) in world.surveyed() {
                *total.entry(nr).or_default() += count;
            }
        }
        total.into_iter().collect()
    }

    /// Reports what a restarting node came back without, against what it had answered with.
    ///
    /// Read after the crash image is in place and before the guest restarts.
    fn note_what_came_back(
        &mut self,
        node: NodeId,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<(), KernelError> {
        if self.durability.is_none() {
            return Ok(());
        }
        let files = self.node_world(node).files();
        let lost =
            self.durability.as_mut().map(|watch| watch.restarted(node, &files)).unwrap_or_default();

        for one in lost {
            ctx.emit_for(
                node,
                TaskId::ZERO,
                EventKind::Diagnostic {
                    code: bohr_trace::DURABILITY_CODE.to_owned(),
                    message: format!(
                        "node {} answered with {} bytes of {} and came back with {}",
                        node.get(),
                        one.answered,
                        one.file,
                        one.came_back
                    ),
                },
            )?;
        }
        Ok(())
    }

    /// Records that a node told somebody something while its disk was dirty.
    ///
    /// The protocol is not read, so an answer is the moment a node reaches outward with unsynced
    /// writes. See `watch::Durability`. It is a domain event.
    fn note_an_answer(
        &mut self,
        node: NodeId,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<(), KernelError> {
        if self.durability.is_none() || !self.disks.get(&node).is_some_and(NodeDisk::is_dirty) {
            return Ok(());
        }
        // The node must also have written since it last read a socket, so replies to reads are not
        // promises. A background write answered later is missed; a false mark would cost more.
        if !self.wrote_since_asking.contains(&node) {
            return Ok(());
        }
        let files = self.node_world(node).files();
        let unsynced: u64 = files.iter().map(|(_, len)| *len).sum();
        if let Some(watch) = self.durability.as_mut() {
            watch.answered(node, &files);
        }
        ctx.emit(EventKind::Domain {
            ns: domain_ns(),
            kind: DOMAIN_ANSWERED,
            payload: unsynced.to_le_bytes().to_vec(),
        })
    }

    /// A node's world and its disk, both made here on the first sight of the node.
    ///
    /// The one place a node is born, with its disk baseline taken before any write.
    ///
    /// Built on demand, since a prepared filesystem is materialised in full.
    fn node_parts(&mut self, node: NodeId) -> (&mut MinimalWorld, &mut NodeDisk) {
        let Self { nodes, disks, template, .. } = self;
        let world = nodes.entry(node).or_insert_with(|| fresh_node(node, template));
        let disk = disks.entry(node).or_insert_with(|| NodeDisk::new(world));
        (world, disk)
    }

    fn node_world(&mut self, node: NodeId) -> &mut MinimalWorld {
        // Registered at birth, so a node that never syncs is noticed. The probe is excluded.
        if let Some(watch) = self.liveness.as_mut()
            && Some(node) != self.probe_node
        {
            watch.knows(node);
        }
        self.node_parts(node).0
    }

    /// Everything a power cut does to one node: the disk falls back to what was synced — torn
    /// tail included — and the process state dies with the process.
    fn crash(
        &mut self,
        node: NodeId,
        ctx: &mut WorldCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        // Recorded before the crash, attributed explicitly since deadlines have no current task.
        if let Some(watch) = self.durability.as_ref() {
            let (files, bytes) = watch.outstanding(node);
            let mut payload = files.to_le_bytes().to_vec();
            payload.extend_from_slice(&bytes.to_le_bytes());
            ctx.emit_for(
                node,
                TaskId::ZERO,
                EventKind::Domain { ns: domain_ns(), kind: DOMAIN_EXPOSED, payload },
            )?;
        }
        // A torn and a reordered write come from one regime, the per-sector landing, so an
        // ablation of either takes the regime away; the crash's draw is unchanged.
        let off = self.fault_policy.suppressed;
        let reorder = self.disk_config.reorder_unsynced
            && !off.contains(FaultKind::DiskTear)
            && !off.contains(FaultKind::DiskReorder);
        let (world, disk) = self.node_parts(node);
        let image = disk.crash_image(node, world, reorder, ctx)?;
        self.nodes.insert(node, restored_node(&self.template, image));
        self.node_of_task.retain(|_, n| *n != node);
        self.net.drop_node(node, wakes);
        self.epolls.drop_node(node);
        Ok(())
    }

    /// Answers the calls the disk model has an opinion about, on descriptors that are files.
    ///
    /// Data calls are answered by the node's world and recorded as `DiskOp`. `fsync` is answered
    /// here, since durability belongs to this model.
    fn disk_call(
        &mut self,
        node: NodeId,
        nr: u32,
        args: [u64; 6],
        mapped: Option<u64>,
        stop: &Stop,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        let fd = args[0] as i64;
        match nr {
            sys::FSYNC | sys::FDATASYNC => {
                // All three draws happen on every sync.
                let now = ctx.world_now();
                let off = self.fault_policy.suppressed;
                let eio_odds = self.fault_policy.windowed(now, self.disk_config.eio_probability);
                let eio_odds = off.odds(FaultKind::DiskError, eio_odds);
                let stall_odds =
                    self.fault_policy.windowed(now, self.disk_config.stall_probability);
                let stall_odds = off.odds(FaultKind::DiskStall, stall_odds);
                let eio = draw_odds(ctx, eio_odds, DrawSite::DiskError)?;
                let stall = draw_odds(ctx, stall_odds, DrawSite::DiskLatency)?;
                let duration =
                    ctx.draw_exponential(self.disk_config.stall_mean_ticks, DrawSite::DiskLatency)?;

                if eio {
                    // As on Linux, this file's dirty pages are dropped (fsyncgate).
                    let (world, disk) = self.node_parts(node);
                    let inode = mapped.or_else(|| world.inode_of(fd));
                    disk.fail_sync(world, inode);
                    ctx.emit(EventKind::FaultFired {
                        fault: FaultKind::DiskError,
                        target: FaultTarget::Descriptor(Fd(fd as i32)),
                    })?;
                    return Self::finish_disk_op(
                        nr,
                        args,
                        fd,
                        DiskOpKind::Fsync,
                        0,
                        0,
                        -errno::EIO,
                        ctx,
                    );
                }
                if stall {
                    let task = ctx.task();
                    let (world, disk) = self.node_parts(node);
                    let inode = mapped.or_else(|| world.inode_of(fd));
                    disk.stall(task, inode);
                    let at = Tick(ctx.world_now().get().saturating_add(duration.max(1)));
                    self.timers.arm(at, node, Due::DiskStallDone { node, task });
                    ctx.emit(EventKind::FaultFired {
                        fault: FaultKind::DiskStall,
                        target: FaultTarget::Descriptor(Fd(fd as i32)),
                    })?;
                    ctx.emit(EventKind::DiskOp {
                        fd: Fd(fd as i32),
                        op: DiskOpKind::Fsync,
                        offset: 0,
                        len: 0,
                        ret: 0,
                    })?;
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: true,
                        data_digest: None,
                    })?;
                    return Ok(Effects::wait(SYSCALL_COST));
                }
                let depth = self.disk_config.snapshot_depth;
                let (world, disk) = self.node_parts(node);
                let inode = mapped.or_else(|| world.inode_of(fd));
                disk.commit(world, inode, depth);
                // Fed when durability happens.
                if let Some(watch) = self.liveness.as_mut() {
                    watch.committed(node);
                }
                if self.durability.is_some() {
                    let files = self.node_world(node).files();
                    if let Some(watch) = self.durability.as_mut() {
                        watch.committed(node, &files);
                    }
                }
                Self::finish_disk_op(nr, args, fd, DiskOpKind::Fsync, 0, 0, 0, ctx)
            }
            sys::WRITE | sys::WRITEV | sys::PWRITE64 | sys::PWRITEV => {
                let enospc_odds = self
                    .fault_policy
                    .windowed(ctx.world_now(), self.disk_config.enospc_probability);
                let enospc_odds =
                    self.fault_policy.suppressed.odds(FaultKind::DiskFull, enospc_odds);
                let enospc = draw_odds(ctx, enospc_odds, DrawSite::DiskError)?;
                let offset = if matches!(nr, sys::PWRITE64 | sys::PWRITEV) {
                    args[3]
                } else {
                    self.node_world(node).file_position(fd)
                };
                // A vectored call passes a segment count, so the length is computed.
                let requested = if matches!(nr, sys::WRITEV | sys::PWRITEV) {
                    MinimalWorld::iov_bytes(ctx, GuestAddr(args[1]), args[2])?
                } else {
                    args[2] as u32
                };
                if enospc {
                    ctx.emit(EventKind::FaultFired {
                        fault: FaultKind::DiskFull,
                        target: FaultTarget::Descriptor(Fd(fd as i32)),
                    })?;
                    return Self::finish_disk_op(
                        nr,
                        args,
                        fd,
                        DiskOpKind::Write,
                        offset,
                        requested,
                        -errno::ENOSPC,
                        ctx,
                    );
                }
                let effects = self.node_world(node).handle(stop, ctx)?;
                let ret = answered(&effects);
                if ret > 0 {
                    // The exact range, for the crash's per-sector decision.
                    let inode = self.node_world(node).inode_of(fd);
                    if let (Some(inode), Some(disk)) = (inode, self.disks.get_mut(&node)) {
                        disk.mark_written(inode, offset, ret as u64);
                    }
                    self.wrote_since_asking.insert(node);
                    // A write-through descriptor is durable at the write; drawn sync failures
                    // apply only to explicit syncs.
                    if let Some(inode) = inode
                        && self.node_world(node).writes_through(fd)
                    {
                        let depth = self.disk_config.snapshot_depth;
                        let (world, disk) = self.node_parts(node);
                        disk.commit(world, Some(inode), depth);
                    }
                }
                ctx.emit(EventKind::DiskOp {
                    fd: Fd(fd as i32),
                    op: DiskOpKind::Write,
                    offset,
                    len: requested,
                    ret,
                })?;
                Ok(effects)
            }
            sys::READ | sys::PREAD64 => {
                let offset = if nr == sys::PREAD64 {
                    args[3]
                } else {
                    self.node_world(node).file_position(fd)
                };
                let effects = self.node_world(node).handle(stop, ctx)?;
                let ret = answered(&effects);
                ctx.emit(EventKind::DiskOp {
                    fd: Fd(fd as i32),
                    op: DiskOpKind::Read,
                    offset,
                    len: args[2] as u32,
                    ret,
                })?;
                Ok(effects)
            }
            sys::FTRUNCATE => {
                let effects = self.node_world(node).handle(stop, ctx)?;
                let ret = answered(&effects);
                // The marks above the new length go with the bytes.
                if ret >= 0 {
                    let inode = self.node_world(node).inode_of(fd);
                    if let (Some(inode), Some(disk)) = (inode, self.disks.get_mut(&node)) {
                        disk.mark_truncated(inode, args[1]);
                    }
                }
                ctx.emit(EventKind::DiskOp {
                    fd: Fd(fd as i32),
                    op: DiskOpKind::Truncate,
                    offset: args[1],
                    len: 0,
                    ret,
                })?;
                Ok(effects)
            }
            // Added space is unsynced, like a write.
            sys::FALLOCATE => {
                let before = self.node_world(node).length_of(fd).unwrap_or(0);
                let effects = self.node_world(node).handle(stop, ctx)?;
                let ret = answered(&effects);
                let after = self.node_world(node).length_of(fd).unwrap_or(0);
                if ret >= 0 && after > before {
                    let inode = self.node_world(node).inode_of(fd);
                    if let (Some(inode), Some(disk)) = (inode, self.disks.get_mut(&node)) {
                        disk.mark_written(inode, before, after - before);
                    }
                }
                ctx.emit(EventKind::DiskOp {
                    fd: Fd(fd as i32),
                    op: DiskOpKind::Truncate,
                    offset: after,
                    len: 0,
                    ret,
                })?;
                Ok(effects)
            }
            _ => Err(ctx.refuse("BOHR-2001", format!("unsupported disk call {nr}"))),
        }
    }

    /// Records the two rows a fully-answered disk call leaves and hands the answer back.
    #[allow(clippy::too_many_arguments)]
    fn finish_disk_op(
        nr: u32,
        args: [u64; 6],
        fd: i64,
        op: DiskOpKind,
        offset: u64,
        len: u32,
        ret: i64,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        ctx.emit(EventKind::DiskOp { fd: Fd(fd as i32), op, offset, len, ret })?;
        ctx.emit(EventKind::Syscall { nr, args, ret, emulated: true, data_digest: None })?;
        Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST))
    }

    /// Draws whether this allocation is refused, where a scenario says memory can run out.
    ///
    /// These calls stop only when `enomem_probability` is set; unrefused ones run in the kernel.
    /// The probe is never refused.
    fn maybe_exhaust(
        &mut self,
        node: NodeId,
        nr: u32,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Option<Effects>, KernelError> {
        /// `brk`, `mmap`, `mremap`.
        const BRK: u32 = 12;
        const MMAP: u32 = 9;
        const MREMAP: u32 = 25;
        /// `MAP_ANONYMOUS`: an allocation.
        const MAP_ANONYMOUS: u64 = 0x20;
        let allocation = nr == BRK || nr == MREMAP || (nr == MMAP && args[3] & MAP_ANONYMOUS != 0);
        if !allocation || self.fault_policy.enomem.0 == 0 {
            return Ok(None);
        }
        // The probe's allocations pass without a draw. `brk(0)` allocates nothing.
        let odds = self.fault_policy.windowed(ctx.world_now(), self.fault_policy.enomem);
        let odds = self.fault_policy.suppressed.odds(FaultKind::MemoryExhausted, odds);
        let refused = Some(node) != self.probe_node
            && !(nr == BRK && args[0] == 0)
            && draw_odds(ctx, odds, DrawSite::MemoryExhausted)?;
        if !refused {
            ctx.emit(EventKind::Syscall { nr, args, ret: 0, emulated: false, data_digest: None })?;
            return Ok(Some(Effects::go(Resume::Continue, SYSCALL_COST)));
        }
        ctx.emit(EventKind::FaultFired {
            fault: FaultKind::MemoryExhausted,
            target: FaultTarget::Node(node),
        })?;
        if nr == BRK {
            // The kernel refuses `brk` by not moving the break.
            ctx.emit(EventKind::Syscall { nr, args, ret: 0, emulated: false, data_digest: None })?;
            return Ok(Some(Effects::go(Resume::Rewrite { nr: BRK, args: [0; 6] }, SYSCALL_COST)));
        }
        let ret = -errno::ENOMEM;
        ctx.emit(EventKind::Syscall { nr, args, ret, emulated: true, data_digest: None })?;
        Ok(Some(Effects::go(Resume::Emulate { ret }, SYSCALL_COST)))
    }

    /// Draws whether this node dies at this moment, where a campaign armed drawn faults.
    ///
    /// Drawn at every syscall; `guided` only zeroes the odds where a fault costs nothing. See
    /// `docs/API.md` §5.4.
    fn maybe_kill(
        &mut self,
        node: NodeId,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Option<Effects>, KernelError> {
        if !self.fault_policy.is_armed() || Some(node) == self.probe_node {
            return Ok(None);
        }
        // Guided targeting aims at unsynced writes.
        let candidate = self.disks.get(&node).is_some_and(NodeDisk::is_dirty);
        let odds = self.fault_policy.odds_at(candidate, ctx.world_now(), self.faults_fired);
        if !draw_odds(ctx, odds, DrawSite::NodeFault)? {
            return Ok(None);
        }
        // The budget counts fires, including those on unsuitable kinds.
        self.faults_fired = self.faults_fired.saturating_add(1);

        // The kind, drawn over the whole enabled set so width never depends on state.
        let kinds = self.fault_policy.kinds.enabled();
        let Some(kind) =
            kinds.get(ctx.draw_index(kinds.len().max(1), DrawSite::NodeFault)?).cloned()
        else {
            return Ok(None);
        };

        // Raised through the network's partition list, like `[[partition]]`. It heals after the
        // recovery delay, or never without one.
        if kind == NodeFaultKind::Isolate {
            if self.fault_policy.suppressed.contains(landed_as(&kind)) {
                return Ok(None);
            }
            let at = ctx.world_now();
            let cut = self.net.links_isolating(node);
            if cut.is_empty() {
                return Ok(None);
            }
            let heals = Tick(at.get().saturating_add(self.fault_policy.restart_after.get()));
            let index = self.net.add_partition(ScheduledPartition {
                at_tick: at,
                heal_at_tick: heals,
                cut,
            });
            self.timers.arm(at, NodeId::ZERO, Due::PartitionChange { index, raise: true });
            if self.fault_policy.restart_after.get() > 0 {
                self.timers.arm(heals, NodeId::ZERO, Due::PartitionChange { index, raise: false });
            }
            return Ok(None);
        }

        // Links stay up but degrade, with a drawn severity.
        if kind == NodeFaultKind::Degrade {
            let at = ctx.world_now();
            let until = Tick(at.get().saturating_add(self.fault_policy.restart_after.get()));
            let bad = &DEGRADATIONS[ctx.draw_index(DEGRADATIONS.len(), DrawSite::NetPartition)?];
            // After the severity's draw, so a suppressed degradation leaves the tape where it was.
            let suppressed = self.fault_policy.suppressed;
            if suppressed.contains(FaultKind::NetDrop) || suppressed.contains(FaultKind::NetDelay) {
                return Ok(None);
            }
            for link in self.net.links_isolating(node) {
                let index = self.net.add_grey(GreyWindow {
                    link,
                    at_tick: at,
                    until_tick: until,
                    drop_probability: bad.drop,
                    latency_factor: bad.latency,
                });
                self.timers.arm(at, NodeId::ZERO, Due::GreyChange { index, open: true });
                if self.fault_policy.restart_after.get() > 0 {
                    self.timers.arm(until, NodeId::ZERO, Due::GreyChange { index, open: false });
                }
            }
            return Ok(None);
        }

        // The size is drawn after the kind; the fault goes through the scenario's fault list.
        let kind = match kind {
            NodeFaultKind::ClockStep { .. } => {
                let span = MAX_DRAWN_CLOCK_STEP.saturating_mul(2).saturating_add(1);
                let drawn = ctx.draw_range(span, DrawSite::ClockSkew)?;
                NodeFaultKind::ClockStep { by_ticks: drawn as i64 - MAX_DRAWN_CLOCK_STEP as i64 }
            }
            NodeFaultKind::ClockDrift { .. } => {
                let span = u64::from(MAX_DRIFT_PPM) * 2 + 1;
                let drawn = ctx.draw_range(span, DrawSite::ClockSkew)?;
                NodeFaultKind::ClockDrift { ppm: drawn as i32 - MAX_DRIFT_PPM as i32 }
            }
            other => other,
        };
        // After every draw the kind makes, so the rest of the run keeps its schedule.
        if self.fault_policy.suppressed.contains(landed_as(&kind)) {
            return Ok(None);
        }

        // A freeze goes through the scenario's fault list, so drawn and written faults share code.
        // The kill is immediate, since its exact moment matters.
        if kind != NodeFaultKind::Kill {
            let at = ctx.world_now();
            self.schedule_fault(at, node, kind.clone());
            if let Some(back) = self.fault_policy.kinds.recovery(&kind) {
                let recovers = Tick(at.get().saturating_add(self.fault_policy.restart_after.get()));
                if self.fault_policy.restart_after.get() > 0 {
                    self.schedule_fault(recovers, node, back);
                }
            }
            return Ok(None);
        }

        ctx.emit(EventKind::FaultFired {
            fault: FaultKind::NodeKill,
            target: FaultTarget::Node(node),
        })?;
        self.node_became(node, NodeState::Dead, ctx.world_now());
        ctx.emit(EventKind::NodeState { state: NodeState::Dead })?;
        // Peers waiting on the killed node's connections read end-of-stream.
        let mut wakes = Vec::new();
        self.crash(node, ctx, &mut wakes)?;

        // Armed like a scheduled restart, through the timer queue and fault list.
        if self.fault_policy.restart_after.get() > 0 {
            // The world clock, as every timer uses.
            let at =
                Tick(ctx.world_now().get().saturating_add(self.fault_policy.restart_after.get()));
            if let Some(back) = self.fault_policy.kinds.recovery(&NodeFaultKind::Kill) {
                // A suppressed restore still brings the node back, from what its disk kept.
                let back = match back {
                    NodeFaultKind::RestartFromSnapshot { .. }
                        if self.fault_policy.suppressed.contains(FaultKind::NodeRestart) =>
                    {
                        NodeFaultKind::Restart
                    }
                    other => other,
                };
                self.schedule_fault(at, node, back);
            }
        }

        Ok(Some(
            Effects::go(Resume::Continue, SYSCALL_COST)
                .commanding(vec![NodeOp::Kill(node)])
                .releasing(wakes),
        ))
    }

    /// Puts a fault in the list a scenario writes its own in, and arms its timer.
    ///
    /// Shared by drawn and scheduled faults.
    fn schedule_fault(&mut self, at: Tick, node: NodeId, kind: NodeFaultKind) {
        self.node_faults.push(ScheduledNodeFault { at_tick: at, node, kind });
        self.timers.arm(at, node, Due::NodeFault { index: self.node_faults.len() - 1 });
    }

    /// Arms drawn node faults for this run.
    #[must_use]
    pub fn with_fault_policy(mut self, policy: FaultPolicy) -> Self {
        self.fault_policy = policy;
        self.net.follow_policy(policy.disarm_after, policy.suppressed);
        // Either order of building: probes set before the policy learn the window here.
        if let (Some(observer), Some(closes)) = (self.observer.as_mut(), policy.disarm_after) {
            observer.judge_stalls_from(closes);
        }
        self
    }

    /// Takes a write to the record channel as records; the guest is told it succeeded.
    fn sdk_write(
        &mut self,
        node: NodeId,
        nr: u32,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        let bytes = if nr == sys::WRITEV {
            MinimalWorld::gather(ctx, GuestAddr(args[1]), args[2])?
        } else {
            let mut buf = vec![0u8; (args[2] as usize).min(MAX_DATAGRAM)];
            ctx.read_mem(GuestAddr(args[1]), &mut buf)?;
            buf
        };
        let digested = self.sdk.accept(node, args[0] as i64, &bytes);
        for (code, message) in digested.diagnostics {
            ctx.emit(EventKind::Diagnostic { code, message })?;
        }
        for event in digested.events {
            ctx.emit(event)?;
        }
        if let Some(violation) = digested.violated {
            self.violated.get_or_insert(violation);
        }
        if let Some(observer) = self.observer.as_mut() {
            let now = ctx.world_now();
            let cut = !self.net.fully_healed();
            for round in &digested.probe_rounds {
                for (request, answer) in crate::probe::answers_in(round, observer.requests()) {
                    for broken in observer.answer(now, cut, request, &answer) {
                        ctx.emit_for(
                            NodeId(broken.node),
                            TaskId::ZERO,
                            EventKind::Diagnostic {
                                code: broken.code.to_owned(),
                                message: broken.message,
                            },
                        )?;
                    }
                }
            }
        }

        let ret = bytes.len() as i64;
        ctx.emit(EventKind::Syscall {
            nr,
            args,
            ret,
            emulated: true,
            data_digest: Some(bohr_trace::short_digest(&bytes)),
        })?;
        Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST))
    }

    /// Answers the readiness family: epoll instances and the counters used to wake them.
    ///
    /// Readiness is computed on demand: sockets ask the network, everything else is ready. An
    /// unknown descriptor is reported ready, so the poller reads it and finds out.
    fn poll_call(
        &mut self,
        node: NodeId,
        nr: u32,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        let answer = |ret: i64, ctx: &mut WorldCtx<'_, '_>| -> Result<Effects, KernelError> {
            ctx.emit(EventKind::Syscall { nr, args, ret, emulated: true, data_digest: None })?;
            Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST))
        };

        match nr {
            poll_sys::EPOLL_CREATE1 => {
                let fd = self.epolls.create(node);
                answer(fd, ctx)
            }
            poll_sys::EVENTFD2 => {
                let fd = self.epolls.open_counter(node, args[0]);
                answer(fd, ctx)
            }
            poll_sys::EPOLL_CTL => {
                let (epfd, op, fd) = (args[0] as i64, args[1], args[2] as i64);
                // `EPOLL_CTL_DEL` ignores the event, which glibc passes as null.
                let watch = if args[3] == 0 {
                    (0, 0)
                } else {
                    let mut buf = [0u8; epoll::EVENT_BYTES];
                    ctx.read_mem(GuestAddr(args[3]), &mut buf)?;
                    let events = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let token = u64::from_le_bytes(buf[4..12].try_into().unwrap_or([0; 8]));
                    (events, token)
                };
                let ret =
                    if self.epolls.control(node, epfd, op, fd, watch) { 0 } else { -errno::EBADF };
                answer(ret, ctx)
            }
            poll_sys::EPOLL_WAIT | poll_sys::EPOLL_PWAIT => {
                let epfd = args[0] as i64;
                let (out, max, timeout) = (GuestAddr(args[1]), args[2] as usize, args[3] as i64);
                let ready = self.ready_on(node, epfd);
                if !ready.is_empty() || timeout == 0 {
                    let wrote = Self::write_ready(ctx, out, max, &ready)?;
                    let fds: Vec<i64> = ready[..wrote].iter().map(|r| r.fd).collect();
                    self.epolls.reported(node, epfd, &fds);
                    return answer(wrote as i64, ctx);
                }
                // Nothing ready: park, with a timer only if a timeout was given.
                self.epolls.park(node, epfd, ctx.task(), out, max, false);
                if timeout > 0 {
                    let at =
                        Tick(ctx.world_now().get().saturating_add(
                            (timeout as u64).saturating_mul(Tick::MILLISECOND.get()),
                        ));
                    self.timers.arm(at, node, Due::PollTimeout { node, epfd, task: ctx.task() });
                }
                ctx.emit(EventKind::Syscall {
                    nr,
                    args,
                    ret: 0,
                    emulated: true,
                    data_digest: None,
                })?;
                Ok(Effects::wait(SYSCALL_COST))
            }
            _ => Err(ctx.refuse("BOHR-2001", format!("unsupported readiness call {nr}"))),
        }
    }

    /// Reads or adds to an event counter, which is what wakes a poller from another thread.
    ///
    /// A read empties it; an empty read answers `EAGAIN` instead of blocking.
    fn counter_call(
        &mut self,
        node: NodeId,
        nr: u32,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        let fd = args[0] as i64;
        let ret = if nr == sys::READ {
            match self.epolls.drain_counter(node, fd) {
                Some(0) | None => -errno::EAGAIN,
                Some(held) => {
                    ctx.write_mem(GuestAddr(args[1]), &held.to_le_bytes())?;
                    8
                }
            }
        } else {
            let mut buf = [0u8; 8];
            ctx.read_mem(GuestAddr(args[1]), &mut buf)?;
            if self.epolls.add_to_counter(node, fd, u64::from_le_bytes(buf)) {
                8
            } else {
                -errno::EBADF
            }
        };
        // A counter write wakes parked pollers now.
        let mut wakes = Vec::new();
        if nr != sys::READ && ret > 0 {
            self.wake_ready_pollers(ctx, &mut wakes)?;
        }
        ctx.emit(EventKind::Syscall { nr, args, ret, emulated: true, data_digest: None })?;
        Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST).releasing(wakes))
    }

    /// Hands every parked poller whatever its instance can now report.
    ///
    /// The answer is written before the wake, which carries the count.
    fn wake_ready_pollers(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        // Readiness may have changed: a new epoch.
        self.epolls.bump();
        let instances: Vec<(NodeId, i64)> = self.epolls.parked_instances();
        for (node, epfd) in instances {
            let ready = self.ready_on(node, epfd);
            if ready.is_empty() {
                continue;
            }
            if let Some((task, out, max, poll)) = self.epolls.take_parked(node, epfd) {
                if poll {
                    let answered = Self::write_revents(ctx, task, out, max, &ready)?;
                    self.epolls.close(node, epfd);
                    wakes.push(Wake { task, ret: answered });
                    continue;
                }
                let taken = ready.len().min(max);
                let mut bytes = Vec::with_capacity(taken * epoll::EVENT_BYTES);
                for one in &ready[..taken] {
                    bytes.extend_from_slice(&one.encode());
                }
                if !bytes.is_empty() {
                    ctx.write_mem_for(task, out, &bytes)?;
                }
                let fds: Vec<i64> = ready[..taken].iter().map(|r| r.fd).collect();
                self.epolls.reported(node, epfd, &fds);
                wakes.push(Wake { task, ret: taken as i64 });
            }
        }
        Ok(())
    }

    /// Answers `poll` on a node that holds sockets, from the readiness `epoll` already computes.
    ///
    /// A `poll` is an epoll instance for one call: ready entries answer now, otherwise a private
    /// instance waits for a delivery or the timeout. Unknown descriptors get `POLLNVAL`.
    fn poll_sockets(
        &mut self,
        node: NodeId,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        let (fds, nfds, timeout) = (GuestAddr(args[0]), args[1] as usize, args[2] as i32);
        // Too many entries is `EINVAL`, as in a kernel.
        if nfds > MAX_POLL_ENTRIES {
            let ret = -errno::EINVAL;
            ctx.emit(EventKind::Syscall {
                nr: sys::POLL,
                args,
                ret,
                emulated: true,
                data_digest: None,
            })?;
            return Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST));
        }

        let mut asked = Vec::with_capacity(nfds);
        if nfds > 0 {
            let mut raw = vec![0u8; nfds * 8];
            ctx.read_mem(fds, &mut raw)?;
            for entry in raw.as_chunks::<8>().0 {
                let fd = i64::from(i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]));
                asked.push((fd, u16::from_le_bytes([entry[4], entry[5]])));
            }
        }
        // Each descriptor's answer now, and what it would be watched for if nothing is ready.
        let mut revents = vec![0u16; nfds];
        for (index, (fd, events)) in asked.iter().enumerate() {
            if *fd < 0 {
                continue;
            }
            // The standard three and every descriptor table are checked.
            let known = (0..=2).contains(fd) || self.holds_descriptor(node, *fd);
            if !known {
                revents[index] = POLLNVAL;
                continue;
            }
            let readable = self
                .net
                .readable(node, *fd)
                .or_else(|| self.epolls.counter(node, *fd).map(|held| held > 0))
                .unwrap_or(true);
            if readable && events & (POLLIN | POLLRDNORM) != 0 {
                revents[index] |= events & (POLLIN | POLLRDNORM);
            }
            // Nothing here has a send buffer to fill, as `ready_on` says for epoll.
            revents[index] |= events & (POLLOUT | POLLWRNORM);
        }
        let ready = revents.iter().filter(|r| **r != 0).count();

        if ready > 0 || timeout == 0 {
            let mut bytes = vec![0u8; nfds * 8];
            ctx.read_mem(fds, &mut bytes)?;
            for (index, entry) in bytes.as_chunks_mut::<8>().0.iter_mut().enumerate() {
                entry[6..8].copy_from_slice(&revents[index].to_le_bytes());
            }
            if nfds > 0 {
                ctx.write_mem(fds, &bytes)?;
            }
            let ret = ready as i64;
            ctx.emit(EventKind::Syscall {
                nr: sys::POLL,
                args,
                ret,
                emulated: true,
                data_digest: None,
            })?;
            return Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST));
        }

        // A private instance whose tokens are array indices.
        let epfd = self.epolls.create_hidden(node);
        for (index, (fd, events)) in asked.iter().enumerate() {
            if *fd >= 0 && events & (POLLIN | POLLRDNORM) != 0 {
                self.epolls.control(
                    node,
                    epfd,
                    epoll::CTL_ADD,
                    *fd,
                    (epoll::EPOLLIN, index as u64),
                );
            }
        }
        self.epolls.park(node, epfd, ctx.task(), fds, nfds, true);
        if timeout > 0 {
            let at = Tick(ctx.world_now().get().saturating_add(
                u64::from(timeout.unsigned_abs()).saturating_mul(Tick::MILLISECOND.get()),
            ));
            self.timers.arm(at, node, Due::PollTimeout { node, epfd, task: ctx.task() });
        }
        ctx.emit(EventKind::Syscall {
            nr: sys::POLL,
            args,
            ret: 0,
            emulated: true,
            data_digest: None,
        })?;
        Ok(Effects::wait(SYSCALL_COST))
    }

    /// Writes a woken `poll`'s answer: each entry's `revents`, from what its instance reported.
    fn write_revents(
        ctx: &mut WorldCtx<'_, '_>,
        task: TaskId,
        fds: GuestAddr,
        nfds: usize,
        ready: &[epoll::Ready],
    ) -> Result<i64, KernelError> {
        if nfds == 0 {
            return Ok(0);
        }
        let mut bytes = vec![0u8; nfds * 8];
        ctx.read_mem_for(task, fds, &mut bytes)?;
        for entry in bytes.as_chunks_mut::<8>().0 {
            entry[6..8].copy_from_slice(&0u16.to_le_bytes());
        }
        // Every entry for a ready descriptor is answered, as a kernel does.
        let entries = bytes.as_chunks_mut::<8>().0;
        let became: std::collections::BTreeSet<i32> = ready
            .iter()
            .filter_map(|one| entries.get(one.token as usize))
            .map(|entry| i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]))
            .collect();
        let mut answered = 0;
        for entry in entries.iter_mut() {
            let fd = i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
            // The instance watched only for input.
            let asked = u16::from_le_bytes([entry[4], entry[5]]) & (POLLIN | POLLRDNORM);
            if became.contains(&fd) && asked != 0 {
                entry[6..8].copy_from_slice(&asked.to_le_bytes());
                answered += 1;
            }
        }
        ctx.write_mem_for(task, fds, &bytes)?;
        Ok(answered)
    }

    /// What an instance would report right now.
    fn ready_on(&self, node: NodeId, epfd: i64) -> Vec<epoll::Ready> {
        self.epolls
            .watched(node, epfd)
            .into_iter()
            .filter_map(|(fd, wanted, token)| {
                let readable = self
                    .net
                    .readable(node, fd)
                    .or_else(|| self.epolls.counter(node, fd).map(|held| held > 0))
                    // Anything else is a file, always readable.
                    .unwrap_or(true);
                let mut events = 0;
                if readable && wanted & epoll::EPOLLIN != 0 {
                    events |= epoll::EPOLLIN;
                }
                // There are no send buffers, so writing is always ready.
                if wanted & epoll::EPOLLOUT != 0 {
                    events |= epoll::EPOLLOUT;
                }
                (events != 0).then_some(epoll::Ready { fd, events, token })
            })
            .collect()
    }

    /// Writes a ready list into the guest's buffer and says how many entries it took.
    fn write_ready(
        ctx: &mut WorldCtx<'_, '_>,
        out: GuestAddr,
        max: usize,
        ready: &[epoll::Ready],
    ) -> Result<usize, KernelError> {
        let taken = ready.len().min(max);
        let mut bytes = Vec::with_capacity(taken * epoll::EVENT_BYTES);
        for one in &ready[..taken] {
            bytes.extend_from_slice(&one.encode());
        }
        if !bytes.is_empty() {
            ctx.write_mem(out, &bytes)?;
        }
        Ok(taken)
    }

    /// Answers the socket family, which never reaches the per-node worlds.
    ///
    /// Each answered call is recorded as an emulated `Syscall` event.
    fn socket_call(
        &mut self,
        node: NodeId,
        nr: u32,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        let mut wakes = Vec::new();
        let ret: i64 = match nr {
            socket_sys::SOCKET => {
                let nonblocking = args[1] & socket_sys::SOCK_NONBLOCK != 0;
                if args[1] & socket_sys::SOCK_KIND == socket_sys::SOCK_STREAM {
                    self.net.open_stream(node, nonblocking)
                } else {
                    self.net.open_udp(node, ctx.task(), nonblocking)
                }
            }
            socket_sys::BIND => match decode_sockaddr(ctx, GuestAddr(args[1]), node)? {
                Some((_, port)) => self.net.bind(node, args[0] as i64, port),
                None => -errno::EINVAL,
            },
            socket_sys::LISTEN => self.net.listen(node, args[0] as i64),
            socket_sys::ACCEPT | socket_sys::ACCEPT4 => {
                let want = PendingAcceptArgs {
                    addr: args[1],
                    addr_len: args[2],
                    nonblocking: nr == socket_sys::ACCEPT4
                        && args[3] & socket_sys::SOCK_NONBLOCK != 0,
                };
                match self.net.accept(node, args[0] as i64, want, ctx)? {
                    Some(ret) => ret,
                    None => return self.park_in(nr, args, ctx),
                }
            }
            socket_sys::CONNECT => match decode_sockaddr(ctx, GuestAddr(args[1]), node)? {
                // A stream's connect creates the connection.
                Some((dest, port)) if self.net.is_stream(node, args[0] as i64) => {
                    let to = Endpoint { node: dest, port };
                    self.net.connect(node, args[0] as i64, to, ctx, &mut wakes)?
                }
                // A datagram connect is remembered for later `send`s, as stub resolvers use.
                Some((dest, port)) => {
                    self.net.connect_datagram(node, args[0] as i64, Endpoint { node: dest, port })
                }
                None => -errno::EINVAL,
            },
            socket_sys::SHUTDOWN => self.net.shutdown(node, args[0] as i64, &mut wakes),
            // Only `SO_REUSEPORT` is remembered; other options are accepted.
            socket_sys::SETSOCKOPT => {
                /// `SOL_SOCKET`, the level the option belongs to.
                const SOL_SOCKET: u64 = 1;
                /// `SO_REUSEPORT`.
                const SO_REUSEPORT: u64 = 15;
                if args[1] == SOL_SOCKET && args[2] == SO_REUSEPORT && args[4] >= 4 {
                    let mut value = [0u8; 4];
                    ctx.read_mem(GuestAddr(args[3]), &mut value)?;
                    let wanted = i32::from_le_bytes(value) != 0;
                    self.net.share_port(node, args[0] as i64, wanted);
                }
                0
            }
            // Options read back zero.
            socket_sys::GETSOCKOPT => {
                if args[4] != 0 {
                    ctx.write_mem(GuestAddr(args[3]), &0i32.to_le_bytes())?;
                    ctx.write_mem(GuestAddr(args[4]), &4i32.to_le_bytes())?;
                }
                0
            }
            socket_sys::GETSOCKNAME => match self.net.name_of(node, args[0] as i64) {
                Some(addr) => {
                    ctx.write_mem(GuestAddr(args[1]), &addr)?;
                    0
                }
                None => -errno::EBADF,
            },
            socket_sys::GETPEERNAME => match self.net.peer_name_of(node, args[0] as i64) {
                Some(addr) => {
                    ctx.write_mem(GuestAddr(args[1]), &addr)?;
                    0
                }
                None => -errno::ENOTCONN,
            },
            socket_sys::SOCKETPAIR => {
                let (a, b) = self.net.open_pair(node);
                let mut pair = [0u8; 8];
                pair[..4].copy_from_slice(&(a as i32).to_le_bytes());
                pair[4..].copy_from_slice(&(b as i32).to_le_bytes());
                ctx.write_mem(GuestAddr(args[3]), &pair)?;
                0
            }
            socket_sys::SENDTO => {
                self.note_an_answer(node, ctx)?;
                let len = (args[2] as usize).min(MAX_DATAGRAM);
                let mut payload = vec![0u8; len];
                ctx.read_mem(GuestAddr(args[1]), &mut payload)?;
                // `send` has no address; a connected socket supplies it.
                let destination = if args[4] == 0 {
                    // A connected datagram socket uses its remembered address.
                    self.net.datagram_peer(node, args[0] as i64).map(|to| (to.node, to.port))
                } else {
                    match decode_sockaddr(ctx, GuestAddr(args[4]), node)? {
                        Some(named) => Some(named),
                        None => {
                            return self.answer_socket_call(nr, args, -errno::EINVAL, wakes, ctx);
                        }
                    }
                };
                match destination {
                    None => {
                        self.net.stream_write(node, args[0] as i64, &payload, ctx, &mut wakes)?
                    }
                    Some((dest_node, dest_port)) => {
                        let mut armed = Vec::new();
                        let ret = self.net.send(
                            node,
                            args[0] as i64,
                            payload,
                            dest_node,
                            dest_port,
                            ctx,
                            &mut |at, to, id| armed.push((at, to, id)),
                        )?;
                        for (at, to, id) in armed {
                            self.timers.arm(at, to, Due::PacketDelivery { id });
                        }
                        ret
                    }
                }
            }
            // Reading the next request closes the previous exchange.
            socket_sys::RECVFROM => {
                self.wrote_since_asking.remove(&node);
                let recv = PendingRecvArgs {
                    buf: GuestAddr(args[1]),
                    len: args[2] as usize,
                    src_addr: args[4],
                };
                match self.net.recv(node, args[0] as i64, recv, ctx)? {
                    Some(ret) => ret,
                    None => return self.park_in(nr, args, ctx),
                }
            }
            sys::CLOSE => self.net.close(node, args[0] as i64, &mut wakes),
            sys::IOCTL if args[1] == FIONBIO => {
                let mut on = [0u8; 4];
                ctx.read_mem(GuestAddr(args[2]), &mut on)?;
                let on = i32::from_le_bytes(on) != 0;
                if self.net.set_nonblocking(node, fd_of(args), on) { 0 } else { -errno::EBADF }
            }
            sys::READ => {
                let recv =
                    PendingRecvArgs { buf: GuestAddr(args[1]), len: args[2] as usize, src_addr: 0 };
                match self.net.recv(node, args[0] as i64, recv, ctx)? {
                    Some(ret) => ret,
                    None => return self.park_in(nr, args, ctx),
                }
            }
            sys::WRITE | sys::WRITEV => {
                // A stream may take less than offered and returns the count.
                let payload = if nr == sys::WRITEV {
                    let mut gathered = MinimalWorld::gather(ctx, GuestAddr(args[1]), args[2])?;
                    gathered.truncate(MAX_DATAGRAM);
                    gathered
                } else {
                    let mut buf = vec![0u8; (args[2] as usize).min(MAX_DATAGRAM)];
                    ctx.read_mem(GuestAddr(args[1]), &mut buf)?;
                    buf
                };
                self.net.stream_write(node, args[0] as i64, &payload, ctx, &mut wakes)?
            }
            _ => return Err(ctx.refuse("BOHR-2001", format!("unsupported socket call {nr}"))),
        };
        self.answer_socket_call(nr, args, ret, wakes, ctx)
    }

    /// Records an answered socket call and hands the answer back.
    ///
    /// Stream writes and connects bypass delivery timers, so pollers are told here.
    fn answer_socket_call(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ret: i64,
        mut wakes: Vec<Wake>,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        self.wake_ready_pollers(ctx, &mut wakes)?;
        ctx.emit(EventKind::Syscall { nr, args, ret, emulated: true, data_digest: None })?;
        Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST).releasing(wakes))
    }

    /// Records a call that parked its task, which answers nothing until something releases it.
    fn park_in(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        ctx.emit(EventKind::Syscall { nr, args, ret: 0, emulated: true, data_digest: None })?;
        Ok(Effects::wait(SYSCALL_COST))
    }

    /// A sleep parks the task and arms a timer; nothing else moves.
    ///
    /// Armed in world ticks; drift shows only in what guests read.
    fn sleep(
        &mut self,
        node: NodeId,
        nr: u32,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        let request = if nr == sys::NANOSLEEP { args[0] } else { args[2] };
        let absolute = nr == sys::CLOCK_NANOSLEEP && args[1] & TIMER_ABSTIME != 0;
        let value = MinimalWorld::read_timespec(ctx, GuestAddr(request))?;
        let ahead = if absolute {
            value.saturating_sub(
                MinimalWorld::deadline_clock(ctx, MinimalWorld::reads_wall(args[0])).get(),
            )
        } else {
            value
        };
        ctx.emit(EventKind::ClockRead { presented: MinimalWorld::presented_now(ctx) })?;

        let at = Tick(ctx.world_now().get().saturating_add(ahead));
        self.timers.arm(at, node, Due::SleepExpiry { task: ctx.task() });
        Ok(Effects::wait(SYSCALL_COST))
    }
}

impl WorldModel for DistributedWorld {
    fn identity(&self) -> WorldModelId {
        if self.template.surveying {
            WorldModelId::new("distributed-survey", MODEL_VERSION)
        } else {
            domain_ns()
        }
    }

    fn handle(&mut self, stop: &Stop, ctx: &mut WorldCtx<'_, '_>) -> Result<Effects, KernelError> {
        let effects = self.answer(stop, ctx)?;
        self.audit_answer(stop, &effects, ctx)?;
        // A finished node is silent by design; read off the effects.
        if let Some(watch) = self.liveness.as_mut() {
            for op in &effects.node_ops {
                if let NodeOp::Ended { node, .. } = op {
                    watch.ended(*node);
                }
            }
        }
        Ok(effects)
    }

    fn task_created(&mut self, task: TaskId, parent: TaskId) {
        let Some(node) = self.node_of_task.get(&parent).copied() else {
            return;
        };
        self.node_of_task.insert(task, node);
        self.node_world(node).task_created(task, parent);
    }

    fn task_exited(&mut self, task: TaskId) -> Vec<Wake> {
        // The network first, so an ended task is never handed a delivery.
        self.net.forget_task(task);
        self.epolls.forget_task(task);
        let Some(node) = self.node_of_task.remove(&task) else {
            return Vec::new();
        };
        self.node_world(node).task_exited(task)
    }

    fn fault_catalog(&self) -> &FaultCatalog {
        &self.catalog
    }

    fn next_deadline(&self) -> Option<Tick> {
        self.timers.next()
    }

    fn on_deadline(&mut self, ctx: &mut WorldCtx<'_, '_>) -> Result<DueEffects, KernelError> {
        let mut effects = DueEffects::default();
        for entry in self.timers.pop_due(ctx.world_now()) {
            match entry {
                Due::SleepExpiry { task } => effects.wake.push(Wake { task, ret: 0 }),
                Due::FutexTimeout { node, task, word, seq } => {
                    // A timeout fires only into the wait that armed it, checked at firing.
                    if self.futex_seq.get(&task) == Some(&seq)
                        && self.nodes.get_mut(&node).is_some_and(|w| w.cancel_wait(task, word))
                    {
                        effects.wake.push(Wake { task, ret: -errno::ETIMEDOUT });
                    }
                }
                Due::PollTimeout { node, epfd, task } => {
                    // A timed-out wait returns an empty list, for this timer's task only and only
                    // if still parked.
                    if let Some((_, out, max, poll)) =
                        self.epolls.take_parked_task(node, epfd, task)
                    {
                        // A timed-out `poll` reports nothing ready.
                        if poll {
                            Self::write_revents(ctx, task, out, max, &[])?;
                            self.epolls.close(node, epfd);
                        }
                        effects.wake.push(Wake { task, ret: 0 });
                    }
                }
                Due::PacketDelivery { id } => {
                    self.net.deliver(id, ctx, &mut effects.wake)?;
                    // A delivery makes a socket readable, so every instance is asked.
                    self.wake_ready_pollers(ctx, &mut effects.wake)?;
                }
                Due::PartitionChange { index, raise } => {
                    self.net.set_partition(index, raise, ctx, &mut effects.wake)?;
                    // A heal resumes streams, so pollers are told.
                    self.wake_ready_pollers(ctx, &mut effects.wake)?;
                    // The window opens when the last cut lifts.
                    if !raise
                        && self.net.fully_healed()
                        && let Some(observer) = self.observer.as_mut()
                    {
                        observer.healed(ctx.world_now());
                    }
                    if !raise
                        && self.net.fully_healed()
                        && let Some(watch) = self.liveness.as_mut()
                    {
                        let due = watch.healed(ctx.world_now());
                        self.timers.arm(due, NodeId::ZERO, Due::LivenessWindow);
                    }
                }
                Due::GreyChange { index, open } => {
                    self.net.set_grey(index, open, ctx)?;
                }
                Due::LivenessWindow => {
                    let now = ctx.world_now();
                    if let Some(found) = self.liveness.as_mut().and_then(|w| w.due(now)) {
                        for node in found.nodes {
                            ctx.emit_for(
                                node,
                                TaskId::ZERO,
                                EventKind::Diagnostic {
                                    code: bohr_trace::LIVENESS_CODE.to_owned(),
                                    message: format!(
                                        "the network healed at tick {} and node {} committed \
                                         nothing in the {} ticks after",
                                        found.healed_at.get(),
                                        node.get(),
                                        found.span.get()
                                    ),
                                },
                            )?;
                        }
                    }
                }
                Due::QuietCheck => {
                    let now = ctx.world_now();
                    let found =
                        self.liveness.as_mut().map(|w| w.gone_quiet(now)).unwrap_or_default();
                    for (node, silent_for) in found {
                        ctx.emit_for(
                            node,
                            TaskId::ZERO,
                            EventKind::Diagnostic {
                                code: bohr_trace::LIVENESS_CODE.to_owned(),
                                message: format!(
                                    "node {} made no call for {} ticks, so it is not idle but \
                                     stopped",
                                    node.get(),
                                    silent_for.get()
                                ),
                            },
                        )?;
                    }
                    // Always re-armed.
                    if let Some(span) = self.liveness.as_ref().and_then(|w| w.quiet_span()) {
                        let at = Tick(now.get().saturating_add(span.get()));
                        self.timers.arm(at, NodeId::ZERO, Due::QuietCheck);
                    }
                }
                Due::DiskStallDone { node, task } => {
                    let depth = self.disk_config.snapshot_depth;
                    let (world, disk) = self.node_parts(node);
                    // A crashed node's stall fires into nothing.
                    if let Some(inflight) = disk.take_inflight(task) {
                        disk.commit(world, inflight.inode, depth);
                        if let Some(watch) = self.liveness.as_mut() {
                            watch.committed(node);
                        }
                        if self.durability.is_some() {
                            let files = self.node_world(node).files();
                            if let Some(watch) = self.durability.as_mut() {
                                watch.committed(node, &files);
                            }
                        }
                        effects.wake.push(Wake { task: inflight.task, ret: 0 });
                    }
                }
                Due::NodeFault { index } => {
                    let Some(fault) = self.node_faults.get(index).cloned() else {
                        return Err(KernelError::Invariant("a node-fault timer names no entry"));
                    };
                    match fault.kind.clone() {
                        // Raised where drawn, through partitions and grey windows.
                        NodeFaultKind::Isolate | NodeFaultKind::Degrade => {
                            return Err(KernelError::Invariant(
                                "a link fault is not a listed node fault",
                            ));
                        }
                        NodeFaultKind::Freeze => {
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::FaultFired {
                                    fault: FaultKind::NodeFreeze,
                                    target: FaultTarget::Node(fault.node),
                                },
                            )?;
                            self.node_became(fault.node, NodeState::Frozen, ctx.world_now());
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::NodeState { state: NodeState::Frozen },
                            )?;
                            effects.node_ops.push(NodeOp::Freeze(fault.node));
                        }
                        NodeFaultKind::ClockStep { by_ticks } => {
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::FaultFired {
                                    fault: FaultKind::ClockStep,
                                    target: FaultTarget::Node(fault.node),
                                },
                            )?;
                            let clock = ctx.node_clock(fault.node).unwrap_or_default();
                            ctx.set_node_clock(fault.node, clock.stepped(by_ticks));
                        }
                        NodeFaultKind::ClockDrift { ppm } => {
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::FaultFired {
                                    fault: FaultKind::ClockDrift,
                                    target: FaultTarget::Node(fault.node),
                                },
                            )?;
                            let clock = ctx.node_clock(fault.node).unwrap_or_default();
                            ctx.set_node_clock(fault.node, clock.rebased(ctx.world_now(), ppm));
                        }
                        NodeFaultKind::Thaw => {
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::FaultFired {
                                    fault: FaultKind::NodeThaw,
                                    target: FaultTarget::Node(fault.node),
                                },
                            )?;
                            self.node_became(fault.node, NodeState::Running, ctx.world_now());
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::NodeState { state: NodeState::Running },
                            )?;
                            effects.node_ops.push(NodeOp::Thaw(fault.node));
                        }
                        NodeFaultKind::Kill => {
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::FaultFired {
                                    fault: FaultKind::NodeKill,
                                    target: FaultTarget::Node(fault.node),
                                },
                            )?;
                            self.node_became(fault.node, NodeState::Dead, ctx.world_now());
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::NodeState { state: NodeState::Dead },
                            )?;
                            self.crash(fault.node, ctx, &mut effects.wake)?;
                            effects.node_ops.push(NodeOp::Kill(fault.node));
                        }
                        NodeFaultKind::Restart => {
                            self.note_what_came_back(fault.node, ctx)?;
                            self.node_became(fault.node, NodeState::Restarting, ctx.world_now());
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::NodeState { state: NodeState::Restarting },
                            )?;
                            effects.node_ops.push(NodeOp::Restart(fault.node));
                        }
                        // Every node at once, in node order.
                        NodeFaultKind::ChainReorg { back, nodes } => {
                            let targets: Vec<NodeId> = if nodes.is_empty() {
                                // Never the probe.
                                let mut every: Vec<NodeId> = self
                                    .nodes
                                    .keys()
                                    .copied()
                                    .filter(|n| Some(*n) != self.probe_node)
                                    .collect();
                                every.sort_unstable();
                                every
                            } else {
                                nodes
                            };

                            // Narrated once, for all nodes.
                            ctx.emit_for(
                                NodeId::ZERO,
                                TaskId::ZERO,
                                EventKind::FaultFired {
                                    fault: FaultKind::ChainReorg,
                                    target: FaultTarget::World,
                                },
                            )?;

                            // Each node dies and restarts, since its descriptors point into the
                            // old world.
                            let mut applied = 0usize;
                            for node in &targets {
                                if let Some(observer) = self.observer.as_mut() {
                                    observer.rewound(node.get());
                                }
                                let (_, disk) = self.node_parts(*node);
                                applied = applied.max(disk.rewind(back));
                                let image = disk.durable_image();

                                self.nodes.insert(*node, restored_node(&self.template, image));
                                self.node_of_task.retain(|_, n| n != node);
                                self.net.drop_node(*node, &mut effects.wake);

                                self.node_became(*node, NodeState::Dead, ctx.world_now());
                                ctx.emit_for(
                                    *node,
                                    TaskId::ZERO,
                                    EventKind::NodeState { state: NodeState::Dead },
                                )?;
                                effects.node_ops.push(NodeOp::Kill(*node));
                            }

                            // The actual depth, which clamping may reduce.
                            let mut payload = (back as u32).to_le_bytes().to_vec();
                            payload.extend_from_slice(&(applied as u32).to_le_bytes());
                            payload.extend_from_slice(&(targets.len() as u32).to_le_bytes());
                            ctx.emit_for(
                                NodeId::ZERO,
                                TaskId::ZERO,
                                EventKind::Domain { ns: domain_ns(), kind: DOMAIN_REORG, payload },
                            )?;

                            for node in targets {
                                self.node_became(node, NodeState::Restarting, ctx.world_now());
                                ctx.emit_for(
                                    node,
                                    TaskId::ZERO,
                                    EventKind::NodeState { state: NodeState::Restarting },
                                )?;
                                effects.node_ops.push(NodeOp::Restart(node));
                            }
                        }
                        NodeFaultKind::RestartFromSnapshot { back } => {
                            self.note_what_came_back(fault.node, ctx)?;
                            if let Some(observer) = self.observer.as_mut() {
                                observer.rewound(fault.node.get());
                            }
                            let (_, disk) = self.node_parts(fault.node);
                            disk.rewind(back);
                            let image = disk.durable_image();
                            self.nodes.insert(fault.node, restored_node(&self.template, image));
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::FaultFired {
                                    fault: FaultKind::NodeRestart,
                                    target: FaultTarget::Node(fault.node),
                                },
                            )?;
                            self.node_became(fault.node, NodeState::Restarting, ctx.world_now());
                            ctx.emit_for(
                                fault.node,
                                TaskId::ZERO,
                                EventKind::NodeState { state: NodeState::Restarting },
                            )?;
                            effects.node_ops.push(NodeOp::Restart(fault.node));
                        }
                    }
                }
            }
        }
        Ok(effects)
    }
}

impl DistributedWorld {
    /// Everything this world does with a stop, which the audit above reads the result of.
    fn answer(&mut self, stop: &Stop, ctx: &mut WorldCtx<'_, '_>) -> Result<Effects, KernelError> {
        let node = ctx.node();
        self.node_of_task.insert(stop.task, node);
        // Any stop shows the node is alive; recorded before answering.
        if let Some(watch) = self.liveness.as_mut()
            && Some(node) != self.probe_node
        {
            watch.stirred(node, ctx.world_now());
        }

        if let StopKind::SyscallEntry { nr, args } = stop.kind {
            if let Some(killed) = self.maybe_kill(node, ctx)? {
                return Ok(killed);
            }
            if let Some(answered) = self.maybe_exhaust(node, nr, args, ctx)? {
                return Ok(answered);
            }
            if nr == sys::IO_URING_ENTER && self.node_world(node).holds_ring(args[0] as i64) {
                return self.enter_ring(node, stop, args, ctx);
            }
        }
        self.route(node, stop, ctx)
    }

    /// Answers `io_uring_enter` on a node: each entry by the route its plain call takes.
    ///
    /// Each entry goes through [`Self::route`] like its plain call. The kill is drawn once per
    /// `io_uring_enter`.
    fn enter_ring(
        &mut self,
        node: NodeId,
        stop: &Stop,
        args: [u64; 6],
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        let ret = match self.node_world(node).take_submissions(ctx, args)? {
            Err(errno) => -errno,
            Ok(taken) => {
                let done = ring::carry_all(&taken.submissions, |asked| {
                    if ring::acts_on_descriptor(asked) && !self.node_world(node).is_file(asked.fd) {
                        return Err(crate::minimal::refuse_submission(
                            ctx,
                            "an operation on a descriptor that is not a file",
                        ));
                    }
                    match ring::plain_call(asked) {
                        Err(what) => Err(crate::minimal::refuse_submission(ctx, &what)),
                        Ok(ring::Plain::Nothing) => Ok(0),
                        Ok(ring::Plain::Call(nr, call)) => {
                            let plain = Stop {
                                task: stop.task,
                                kind: StopKind::SyscallEntry { nr, args: call },
                            };
                            let effects = self.route(node, &plain, ctx)?;
                            self.audit_answer(&plain, &effects, ctx)?;
                            crate::minimal::submission_answer(ctx, &effects)
                        }
                    }
                })?;
                self.node_world(node).post_completions(ctx, taken.fd, &done)?;
                taken.submissions.len() as i64
            }
        };
        ctx.emit(EventKind::Syscall {
            nr: sys::IO_URING_ENTER,
            args,
            ret,
            emulated: true,
            data_digest: None,
        })?;
        Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST))
    }

    /// Answers a stop by what it asks for, once the kill has been drawn.
    fn route(
        &mut self,
        node: NodeId,
        stop: &Stop,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Effects, KernelError> {
        if let StopKind::SyscallEntry { nr, args } = stop.kind {
            if nr == sys::NANOSLEEP || nr == sys::CLOCK_NANOSLEEP {
                return self.sleep(node, nr, args, ctx);
            }

            let poll_family = matches!(
                nr,
                poll_sys::EPOLL_CREATE1
                    | poll_sys::EPOLL_CTL
                    | poll_sys::EPOLL_WAIT
                    | poll_sys::EPOLL_PWAIT
                    | poll_sys::EVENTFD2
            );
            if poll_family {
                return self.poll_call(node, nr, args, ctx);
            }
            // `poll` on a node with sockets, pollers or counters is answered here.
            if nr == sys::POLL
                && (self.net.node_has_sockets(node) || self.epolls.node_holds_any(node))
            {
                return self.poll_sockets(node, args, ctx);
            }
            // A counter reads eight bytes and adds on write.
            if matches!(nr, sys::READ | sys::WRITE) && self.epolls.is_counter(node, args[0] as i64)
            {
                return self.counter_call(node, nr, args, ctx);
            }
            // `fcntl` on this model's descriptors is answered here, as runtimes set them
            // non-blocking.
            if nr == sys::FCNTL
                && (self.net.is_socket(node, args[0] as i64)
                    || self.epolls.is_counter(node, args[0] as i64)
                    || args[0] as i64 >= epoll::EPOLL_FD_BASE)
            {
                let ret = match args[1] {
                    // Nothing here survives exec or blocks.
                    F_GETFD | F_SETFD | F_SETFL => 0,
                    F_GETFL => O_RDWR | O_NONBLOCK,
                    // Copied by whichever disjoint table holds the original.
                    F_DUPFD | F_DUPFD_CLOEXEC => self
                        .epolls
                        .dup(node, args[0] as i64)
                        .or_else(|| self.net.dup(node, args[0] as i64))
                        .unwrap_or(-errno::EBADF),
                    _ => -errno::EINVAL,
                };
                ctx.emit(EventKind::Syscall { nr, args, ret, emulated: true, data_digest: None })?;
                return Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST));
            }
            if nr == sys::CLOSE && self.epolls.close(node, args[0] as i64) {
                ctx.emit(EventKind::Syscall {
                    nr,
                    args,
                    ret: 0,
                    emulated: true,
                    data_digest: None,
                })?;
                return Ok(Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST));
            }

            // A pipe is a stream pair. Direction is not enforced: a write to the read end is
            // delivered where the kernel would answer `EBADF`.
            if nr == sys::PIPE2 {
                let (read_end, write_end) = self.net.open_pair(node);
                if args[1] as i64 & O_NONBLOCK != 0 {
                    self.net.set_nonblocking(node, read_end, true);
                    self.net.set_nonblocking(node, write_end, true);
                }
                let mut pair = [0u8; 8];
                pair[..4].copy_from_slice(&(read_end as i32).to_le_bytes());
                pair[4..].copy_from_slice(&(write_end as i32).to_le_bytes());
                ctx.write_mem(GuestAddr(args[0]), &pair)?;
                ctx.emit(EventKind::Syscall {
                    nr,
                    args,
                    ret: 0,
                    emulated: true,
                    data_digest: None,
                })?;
                return Ok(Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST));
            }

            let ioctl_on_socket = nr == sys::IOCTL && self.net.is_socket(node, args[0] as i64);
            let socket_family = matches!(
                nr,
                socket_sys::SOCKET
                    | socket_sys::CONNECT
                    | socket_sys::SENDTO
                    | socket_sys::RECVFROM
                    | socket_sys::SHUTDOWN
                    | socket_sys::BIND
                    | socket_sys::GETSOCKNAME
                    | socket_sys::GETPEERNAME
                    | socket_sys::SOCKETPAIR
                    | socket_sys::SETSOCKOPT
                    | socket_sys::GETSOCKOPT
                    | socket_sys::SENDMSG
                    | socket_sys::RECVMSG
                    | socket_sys::LISTEN
                    | socket_sys::ACCEPT
                    | socket_sys::ACCEPT4
            );
            // read/write/writev/close follow the descriptor: sockets here, files to the node.
            #[cfg(feature = "chaos-unknown-descriptor")]
            let vectored = crate::chaos::vectored_reaches_sockets();
            #[cfg(not(feature = "chaos-unknown-descriptor"))]
            let vectored = true;

            let on_socket_fd = (matches!(nr, sys::READ | sys::WRITE | sys::CLOSE)
                || (vectored && nr == sys::WRITEV))
                && self.net.is_socket(node, args[0] as i64);
            if socket_family || on_socket_fd || ioctl_on_socket {
                return self.socket_call(node, nr, args, ctx);
            }

            // Record channel writes are records, checked first.
            if matches!(nr, sys::WRITE | sys::WRITEV)
                && self.sdk_fds.contains(&(node, args[0] as i64))
            {
                return self.sdk_write(node, nr, args, ctx);
            }

            // Standard output and error go to the cluster sink. Too many segments are refused
            // before any is read.
            if nr == sys::WRITEV && crate::minimal::too_many_iovecs(args[2]) {
                let ret = -crate::nums::errno::EINVAL;
                return Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST));
            }
            if matches!(nr, sys::WRITE | sys::WRITEV) && (args[0] == 1 || args[0] == 2) {
                let bytes = if nr == sys::WRITEV {
                    MinimalWorld::gather(ctx, GuestAddr(args[1]), args[2])?
                } else {
                    let mut buf = vec![0u8; (args[2] as usize).min(MAX_DATAGRAM)];
                    ctx.read_mem(GuestAddr(args[1]), &mut buf)?;
                    buf
                };
                if let Some(sink) = self.output.as_mut() {
                    use std::io::Write as _;
                    let _ = sink.write_all(&bytes);
                    let _ = sink.flush();
                }
                // Kept on the writing node, where projections read.
                self.node_world(node).keep_written_out(&bytes);
                if let Some(tails) = self.tails.as_mut() {
                    tails.wrote(node.get(), &bytes);
                }
                let ret = bytes.len() as i64;
                ctx.emit(EventKind::Syscall {
                    nr,
                    args,
                    ret,
                    emulated: true,
                    data_digest: Some(bohr_trace::short_digest(&bytes)),
                })?;
                return Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST));
            }

            let disk_shaped = matches!(
                nr,
                sys::WRITE
                    | sys::WRITEV
                    | sys::PWRITE64
                    | sys::PWRITEV
                    | sys::READ
                    | sys::PREAD64
                    | sys::FSYNC
                    | sys::FDATASYNC
                    | sys::FTRUNCATE
                    | sys::FALLOCATE
            );
            if disk_shaped && self.node_world(node).is_file(args[0] as i64) {
                return self.disk_call(node, nr, args, None, stop, ctx);
            }

            // `msync` takes the same path as `fsync`: same draws, failures and stalls.
            if nr == sys::MSYNC {
                return match self.node_world(node).mapped_sync(args[0], args[1]) {
                    Err(()) => Err(ctx.refuse(
                        "BOHR-2001",
                        "one sync over mappings of several files is not modelled; \
                         see docs/PLATFORMS.md §4.3",
                    )),
                    Ok(None) => {
                        ctx.emit(EventKind::Syscall {
                            nr,
                            args,
                            ret: 0,
                            emulated: true,
                            data_digest: None,
                        })?;
                        Ok(Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST))
                    }
                    Ok(Some((fd, inode))) => {
                        for (offset, len) in self.node_world(node).collect_mapped(ctx, inode)? {
                            let (_, disk) = self.node_parts(node);
                            disk.mark_written(inode, offset, len);
                        }
                        self.disk_call(
                            node,
                            sys::FSYNC,
                            [fd as u64, 0, 0, 0, 0, 0],
                            Some(inode),
                            stop,
                            ctx,
                        )
                    }
                };
            }

            // The timeout is read before delegation and armed only if the task parked.
            let base = futex_op::base(args[1] as u32);
            let wait =
                nr == sys::FUTEX && (base == futex_op::WAIT || base == futex_op::WAIT_BITSET);
            // Every wait is numbered, so old timers cannot end it.
            let seq = if wait {
                let seq = self.futex_seq.get(&ctx.task()).map_or(1, |n| n + 1);
                self.futex_seq.insert(ctx.task(), seq);
                seq
            } else {
                0
            };
            let timed_wait = wait && args[3] != 0;
            if timed_wait {
                let value = MinimalWorld::read_timespec(ctx, GuestAddr(args[3]))?;
                // WAIT carries a duration, WAIT_BITSET an absolute deadline on the node's clock,
                // its wall clock where the operation carries `FUTEX_CLOCK_REALTIME`.
                let wall = args[1] as u32 & futex_op::CLOCK_REALTIME != 0;
                let ahead = if base == futex_op::WAIT_BITSET {
                    value.saturating_sub(MinimalWorld::deadline_clock(ctx, wall).get())
                } else {
                    value
                };
                let effects = self.node_world(node).handle(stop, ctx)?;
                if matches!(effects.state, TaskState::Blocked) {
                    let at = Tick(ctx.world_now().get().saturating_add(ahead));
                    self.timers.arm(
                        at,
                        node,
                        Due::FutexTimeout { node, task: ctx.task(), word: args[0], seq },
                    );
                }
                return Ok(effects);
            }
        }

        let effects = self.node_world(node).handle(stop, ctx)?;

        // Opening the record path makes the record channel.
        if let StopKind::SyscallEntry { nr, args } = stop.kind {
            if nr == sys::OPENAT
                && let (
                    true,
                    TaskState::Runnable(
                        Resume::Emulate { ret } | Resume::EmulateAndSignal { ret, .. },
                    ),
                ) = (self.sdk_path.is_some(), &effects.state)
            {
                let opened = if *ret >= 0 { opened_path(ctx, GuestAddr(args[1]))? } else { None };
                if opened.is_some() && opened == self.sdk_path {
                    self.sdk_fds.insert((node, *ret));
                }
            }
            // Closing the record channel ends it, since its number is reused.
            if nr == sys::CLOSE {
                self.sdk_fds.remove(&(node, args[0] as i64));
            }
        }
        Ok(effects)
    }

    /// Whether this world has just told a guest it never issued a descriptor it did issue.
    ///
    /// `EBADF` for a held descriptor tells the guest something untrue, so the run stops.
    ///
    /// Only for calls whose first argument is a descriptor this world owns.
    fn audit_answer(
        &mut self,
        stop: &Stop,
        effects: &Effects,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<(), KernelError> {
        let StopKind::SyscallEntry { nr, args } = stop.kind else {
            return Ok(());
        };
        if !takes_a_descriptor(nr) {
            return Ok(());
        }
        let TaskState::Runnable(Resume::Emulate { ret } | Resume::EmulateAndSignal { ret, .. }) =
            effects.state
        else {
            return Ok(());
        };
        if ret != -errno::EBADF {
            return Ok(());
        }
        let node = ctx.node();
        let fd = args[0] as i64;
        if !self.holds_descriptor(node, fd) {
            return Ok(());
        }
        Err(KernelError::World {
            code: disk::IMPOSSIBLE_STATE,
            message: format!(
                "call {nr} on node {} was answered EBADF for descriptor {fd}, which this world \
                 issued and has not taken back. Whatever the guest does next is this world's \
                 doing, so the run is not evidence about the guest and is refused rather than \
                 reported",
                node.0
            ),
        })
    }

    /// Whether the descriptor is one this world issued to that node and still knows.
    ///
    /// Every table, since the guest sees one descriptor namespace.
    fn holds_descriptor(&mut self, node: NodeId, fd: i64) -> bool {
        if fd < 0 {
            return false;
        }
        self.sdk_fds.contains(&(node, fd))
            || self.net.is_socket(node, fd)
            || self.epolls.holds(node, fd)
            || self.node_world(node).holds_fd(fd)
    }
}

/// Whether this call's first argument is a descriptor.
///
/// Listed by name; `openat`'s first argument is a directory handle.
const fn takes_a_descriptor(nr: u32) -> bool {
    matches!(
        nr,
        sys::READ
            | sys::WRITE
            | sys::WRITEV
            | sys::PWRITEV
            | sys::CLOSE
            | sys::LSEEK
            | sys::FSTAT
            | sys::FTRUNCATE
            | sys::FSYNC
            | sys::FDATASYNC
            | sys::FLOCK
            | sys::FCNTL
            | sys::FSTATFS
            | sys::IOCTL
            | socket_sys::CONNECT
            | socket_sys::SENDTO
            | socket_sys::RECVFROM
            | socket_sys::SHUTDOWN
            | socket_sys::BIND
            | socket_sys::GETSOCKNAME
            | socket_sys::GETPEERNAME
            | socket_sys::SETSOCKOPT
            | socket_sys::GETSOCKOPT
            | socket_sys::SENDMSG
            | socket_sys::RECVMSG
            | socket_sys::LISTEN
            | socket_sys::ACCEPT
            | socket_sys::ACCEPT4
            | poll_sys::EPOLL_CTL
            | poll_sys::EPOLL_WAIT
            | poll_sys::EPOLL_PWAIT
    )
}

impl std::fmt::Debug for DistributedWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DistributedWorld")
            .field("nodes", &self.nodes.len())
            .field("timers", &self.timers)
            .finish_non_exhaustive()
    }
}
