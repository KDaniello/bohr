//! The distributed-systems world: one model, many machines.
//!
//! Every node gets a world of its own — files, descriptors, futex spaces — because two machines
//! share none of those, and the same numeric futex address on two nodes is two different words.
//! What the nodes share is time's arrow and this model's timer queue; sleeps and timed waits park
//! the task and arm a timer instead of advancing the clock on the spot, since one node sleeping an
//! hour must not drag four others' clocks with it.

mod disk;
mod epoll;
pub(crate) mod net;
mod sdk;
mod timers;
mod watch;

use std::collections::{BTreeMap, BTreeSet};

use bohr_backend::{Resume, Stop, StopKind};
use bohr_kernel::{
    DueEffects, Effects, FaultCatalog, KernelError, NodeOp, TaskState, Wake, WorldCtx, WorldModel,
};
use bohr_tape::DrawSite;
use bohr_trace::{DiskOpKind, Endpoint, EventKind, FaultKind, FaultTarget, NodeState};
use bohr_types::{Fd, GuestAddr, NodeId, TaskId, Tick, WorldModelId};
pub use disk::DiskConfig;
use disk::NodeDisk;
use epoll::Epolls;
use net::{decode_sockaddr, Network, PendingAcceptArgs, PendingRecvArgs};
pub use net::{GreyWindow, NetworkConfig, ScheduledPartition, Topology};
use timers::{Due, TimerQueue};
use watch::{Durability, Liveness};
pub use watch::{DurabilityConfig, LivenessConfig};

use crate::{
    minimal::{futex_op, MinimalWorld, SYSCALL_COST, TIMER_ABSTIME},
    nums::{errno, sys},
};

/// How a drawn node fault picks the moment it fires.
///
/// The specification's claim (`docs/API.md` §5.4) is that aiming at dangerous moments finds an
/// order of magnitude more than firing uniformly. It is a claim about search effectiveness, so it
/// is measured rather than asserted, and the number goes into `benches/baseline.toml` whatever it
/// turns out to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Targeting {
    /// Any moment is as good as any other.
    #[default]
    Uniform,
    /// Only moments where a fault would cost the guest something.
    ///
    /// For the disk that means: the node has written since its last `fsync`. A crash there loses
    /// an acknowledged write if the guest acknowledged before syncing, and loses nothing if it
    /// did not — which is exactly the distinction a search is looking for.
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
/// Three rather than a probability drawn digit by digit: what a search wants apart are the classes
/// — slow but whole, lossy but prompt, and both — and a continuum spends tape telling apart runs
/// that behave the same way.
const DEGRADATIONS: [Degradation; 3] = [
    Degradation { drop: None, latency: Some(32) },
    Degradation { drop: Some((1, 4)), latency: None },
    Degradation { drop: Some((1, 2)), latency: Some(8) },
];

/// Which node faults a campaign may draw.
///
/// **Only what a running node can be given**, because a fault is drawn at a syscall and a node that
/// is making one is running: a thaw or a restart drawn there would be aimed at a state the node is
/// not in. Coming back is not a kind of its own here — it is the second half of the fault that took
/// the node away, armed the way the restart after a kill has always been.
///
/// A fixed set of flags rather than a list, so the policy stays `Copy` and the order the kinds are
/// indexed in belongs to this type rather than to the order somebody wrote them in a file.
///
/// **A campaign that enables one kind consumes exactly what it did before.** An index into a set of
/// one is a choice with one option, and a choice with one option costs no tape (`docs/API.md`
/// §2.4) — so arming only the kill leaves every stored bundle replaying to the digest it recorded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DrawnFaults {
    /// Power off: every task dies and everything unsynced is lost. Comes back as a restart.
    pub kill: bool,
    /// Pause without loss, the way a garbage-collection pause looks from outside. Comes back as a
    /// thaw.
    pub freeze: bool,
    /// Cut every link between this node and the rest, and heal them together.
    ///
    /// A whole node rather than a link of a pair chosen by a draw: isolation is what a campaign
    /// wants far more often, and it needs nothing drawn beyond the node the fault was aimed at —
    /// so arming it costs a fire and not a byte, which is what keeps a scenario that draws one
    /// kind laying its tape out as it did.
    pub isolate: bool,
    /// Degrade every link of a node without cutting one, and let them recover together.
    ///
    /// The other half of the cut above: a partition a guest can see against a link that merely
    /// stopped being good, which is the failure most clusters handle worst and the one no scenario
    /// could search for — a grey window stood where somebody wrote its tick down.
    pub degrade: bool,
    /// Whether a killed node comes back on the sync before its last one rather than on its last.
    ///
    /// Not a kind but a choice about the recovery, because that is what it is: the fault is still
    /// the kill, and what changes is the disk the node finds when it returns. One step back and
    /// never a drawn depth — a depth would be a second draw at this site, and a site whose width
    /// depends on what it drew first is a site a mutation cannot edit without moving everything
    /// after it. A scenario wanting deeper writes the tick down.
    pub recover_from_snapshot: bool,
}

impl Default for DrawnFaults {
    /// The kill alone, recovering on the disk its crash left — what every campaign before this
    /// could draw, and what keeps their tapes laid out as they were.
    fn default() -> Self {
        Self {
            kill: true,
            freeze: false,
            isolate: false,
            degrade: false,
            recover_from_snapshot: false,
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
            // Carried as a kind so the draw is one index over one set. What it does is not a node
            // lifecycle event, so it is answered where the fire is rather than through the fault
            // list — see `maybe_kill`.
            kinds.push(NodeFaultKind::Isolate);
        }
        if self.degrade {
            kinds.push(NodeFaultKind::Degrade);
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
    /// How the moment is chosen.
    pub targeting: Targeting,
    /// Ticks after a drawn kill before the node is started again. Zero leaves it dead.
    ///
    /// A crash nobody recovers from tests half of what a crash tests. The interesting question is
    /// what the node finds when it comes back — whether what it told a client was durable is still
    /// there — and it cannot be asked of a node that never returns.
    pub restart_after: Tick,
    /// Virtual time before which a drawn fault is not offered at all.
    ///
    /// **A campaign against a guest that is expensive to start otherwise measures starting it.** A
    /// kill is drawn at one part in the odds per eligible call, and an execution client spends a
    /// hundred and thirty thousand calls coming up: at the finest odds this ABI can express it is
    /// killed, on average, before it has finished. Every run then asks what a crash during startup
    /// does, over and over, and never what a crash to a working node does.
    ///
    /// Zero for every scenario that does not say otherwise, which changes nothing.
    pub arm_after: Tick,
    /// How many drawn faults one run may fire, where a scenario puts a ceiling on it.
    ///
    /// **Odds alone say when a fault fires and never how many.** At one part in a thousand per
    /// eligible call, a guest making millions of them is killed hundreds of times in a run —
    /// measured, two hundred and seventy kills over twenty runs of an execution client — and what
    /// such a run reports on is the restart path, over and over, rather than the one thing a
    /// campaign wanted to know. `arm_after` fixed when a fault may fire; this fixes how often.
    ///
    /// Spent, the policy returns nought odds rather than stopping the draw, for the reason every
    /// other arm here does: consumption is a function of the request and never of the run's state.
    pub budget_per_run: Option<u32>,
    /// Which kinds a fire may be, chosen by a draw over this set.
    pub kinds: DrawnFaults,
}

impl FaultPolicy {
    /// The odds at this moment, given whether it is one the targeting cares about.
    ///
    /// Both arms return odds rather than one of them skipping the draw, because the draw happens
    /// either way: consumption must be a function of the request and not of the state, or one
    /// tape would read from different offsets the moment a disk fell out of step — and a mutation
    /// aimed at one decision would perturb every later one.
    /// Nought before the policy is armed, for the reason the guided arm returns nought rather than
    /// skipping: the draw happens either way, and consumption that depended on the clock would move
    /// every later decision the moment a run took a different amount of time to get anywhere.
    ///
    /// Against the world clock and never a node's own, which carries a drawn offset and drift: a
    /// moment a scenario writes down means the same moment on every node, exactly as a scheduled
    /// fault's does.
    const fn odds_at(self, candidate: bool, now: Tick, fired: u32) -> (u32, u32) {
        if now.get() < self.arm_after.get() {
            return (0, 1);
        }
        if let Some(budget) = self.budget_per_run {
            if fired >= budget {
                return (0, 1);
            }
        }
        match self.targeting {
            Targeting::Uniform => self.kill,
            Targeting::Guided if candidate => self.kill,
            Targeting::Guided => (0, 1),
        }
    }

    /// Whether this policy does anything at all.
    const fn is_armed(self) -> bool {
        self.kill.0 > 0 && self.kill.1 > 0
    }
}

/// A node fault fired by the scenario clock rather than drawn from the tape.
///
/// The scheduled form is what makes a hand-written scenario a regression test; the drawn form
/// arrives with the explorer.
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
    /// Drawn only. A scenario that wants a cut written down says which links in `[[partition]]`,
    /// where it can name one direction of one pair; this is the whole-node case a search wants,
    /// and it exists as a kind so that a fire can be one.
    Isolate,
    /// Make every link of this node bad without breaking it, and let it recover.
    ///
    /// Drawn only, as [`Self::Isolate`], and the counterpart to it: a cut a guest can detect, and
    /// this one it mostly cannot. How bad is drawn too — see `DEGRADATIONS`.
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
    /// The node-level rewind above lifted from the node to the cluster. The world has no notion of
    /// a block and this milestone forbids one, so what a reorg is here is protocol-free: every
    /// node in the set goes back to an older synced image at the same moment, the history
    /// after that point is discarded, and what the cluster does next is a different history
    /// rather than a replay of the same one.
    ChainReorg {
        /// Snapshots to step back from each node's newest.
        back: usize,
        /// The nodes rewound, in order. Empty means every node the cluster has.
        nodes: Vec<NodeId>,
    },
}

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
/// This model's own vocabulary, carried through the extension point the event stream reserves for
/// exactly that (`EventKind::Domain`): the core has no word for an acknowledgement, and inventing
/// one in the core to describe a thing only this world can see is how a shared format stops being
/// shared.
const DOMAIN_ANSWERED: u32 = 1;

/// History was rewound and let run again, with how deep it was asked to go and how deep it went.
///
/// `FaultFired` carries no number, so the depth travels here — and it has to travel somewhere,
/// because a rewind clamped to a node's own history went somewhere other than where it was told.
const DOMAIN_REORG: u32 = 2;

/// What a node had answered for and not yet made durable at the moment a crash took it.
///
/// A measurement and never a verdict. It is what tells a campaign whether its crashes landed
/// anywhere that could cost anything: a thousand clean runs whose crashes all fell on a node with
/// nothing outstanding are a thousand runs that asked no question, and nothing else in the run
/// distinguishes them from a thousand that did.
const DOMAIN_EXPOSED: u32 = 3;

/// This model's version, recorded in every bundle and checked before a replay.
///
/// Incremented when the model's behaviour changes in a way that makes a stored run replay into a
/// different world. Version 2 made durability a promise about one file rather than about the whole
/// disk, and made a crash decide each dirty sector for itself.
const MODEL_VERSION: u16 = 2;

/// The namespace this model's domain events are read in.
///
/// Derived from the model's own version rather than written out, so a reader can never be handed a
/// `kind` number under a version that did not produce it.
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
        TaskState::Runnable(Resume::Emulate { ret }) => ret,
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
/// The node's own reader rather than a fixed-size one: a path sitting within a few bytes of the end
/// of its mapping — which is where the strings a program is started with live — cannot be read as a
/// block, and a supervisor that demanded one would refuse a call the guest was entitled to make.
fn opened_path(ctx: &mut WorldCtx<'_, '_>, addr: GuestAddr) -> Result<Option<String>, KernelError> {
    // Only a name is of interest here: recognising which descriptor a call opened. A call that
    // named nothing opened nothing, and the answer it gets is the node's own to give.
    Ok(match crate::minimal::read_c_string(ctx, addr)? {
        crate::minimal::PathArg::Named(name) => Some(name),
        _ => None,
    })
}

/// True with probability `num/den`, always consuming two bytes.
///
/// Not [`WorldCtx::draw_bool`]: its certainty shortcut consumes nothing for 0 and 1, which is
/// right for configuration that never changes mid-run and wrong for the network and the disk,
/// where an override or a zeroed default must not change what a request costs in tape.
pub(crate) fn draw_odds(
    ctx: &mut WorldCtx<'_, '_>,
    (num, den): (u32, u32),
    site: bohr_tape::DrawSite,
) -> Result<bool, KernelError> {
    let raw = ctx.draw_range(1 << 16, site)?;
    if den == 0 {
        return Ok(false);
    }
    Ok(raw < (u64::from(num) << 16) / u64::from(den))
}

/// Answers every node's syscalls from that node's own world, and owns the cluster's timers.
pub struct DistributedWorld {
    nodes: BTreeMap<NodeId, MinimalWorld>,
    /// Which node each task belongs to, learned from the stop that first names the task. Needed
    /// by the callbacks the kernel makes without a context.
    node_of_task: BTreeMap<TaskId, NodeId>,
    timers: TimerQueue,
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
    /// The one thing that tells an answer apart from a remark. A service reads a request, does
    /// whatever it does, and replies; a reply with a write behind it in the same breath is what an
    /// acknowledgement looks like from outside, and a reply with nothing behind it is the service
    /// saying it had nothing to do.
    wrote_since_asking: BTreeSet<NodeId>,
    /// How a drawn node fault chooses its moment, where a campaign asked for drawn ones.
    fault_policy: FaultPolicy,
    /// How many drawn faults have fired, against the budget a scenario put on the run.
    faults_fired: u32,
    /// An assertion that failed, which makes the run a finding.
    violated: Option<String>,
    /// Where every node's own output goes. One sink for the cluster, because five processes on
    /// one terminal is what a cluster looks like, and the order they reach it is the schedule.
    output: Option<crate::OutputSink>,
    catalog: FaultCatalog,
    /// What every node's world is made of.
    template: NodeTemplate,
    /// Watches for a cluster that healed and went on committing nothing, where one was asked for.
    liveness: Option<Liveness>,
    /// Watches for a node that answered before it synced, where one was asked for.
    durability: Option<Durability>,
    /// Readiness sets and event counters, which span a node's files and its sockets both.
    epolls: Epolls,
}

/// A node's world as this cluster configures one.
///
/// A free function taking the flag rather than a method, so every site that can be the *first* to
/// need a node's world — a syscall, a scheduled fault firing before the guest has made one, a
/// restart after a crash — builds the same thing. A node born down one path and answering
/// differently from one born down another is a difference nothing downstream would notice.
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
    // The record channel is a path this world hands the guest, so this world is what makes
    // somewhere to put it. A guest told to write to a directory it did not choose should not have
    // to make that directory first.
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
    // Every node of a cluster has its waits timed out from here, so a wait carrying a timeout is
    // one this world may park rather than one it must refuse.
    world.with_timeouts_armed()
}

/// A node's world holding a disk it is handed, rather than the one its template would build.
///
/// The prepared filesystem is deliberately not materialised: restoring an image replaces content,
/// names and numbering in full, so building the template's copy first would be a copy of the whole
/// prepared state discarded on the next line.
fn restored_node(template: &NodeTemplate, image: crate::minimal::DiskImage) -> MinimalWorld {
    let mut world = node_shell(template);
    world.restore_disk(image);
    world
}

/// What every node's world is made of, whichever path made it.
///
/// Gathered into one value because there are six places a node's world can first be needed — a
/// syscall, a scheduled fault firing before the guest has made one, a crash, a restart — and a node
/// born down one path answering differently from one born down another is a difference nothing
/// downstream would notice.
#[derive(Clone, Debug, Default)]
struct NodeTemplate {
    /// The filesystem every node starts with, where a scenario prepared one.
    state: Option<std::sync::Arc<crate::StateImage>>,
    /// What `getcwd` answers.
    cwd: Option<String>,
    /// Whether a call this build cannot model is answered rather than refused.
    surveying: bool,
    /// Whether what a node writes out is kept for a projection to read.
    keep_output: bool,
    /// The path a guest is told to write records to, which every node needs somewhere to put.
    sdk_path: Option<String>,
    /// The program each node runs, in node order, for the calls that ask a node where it is.
    ///
    /// Per node rather than one for the cluster: a differential scenario runs a different
    /// implementation on each, and one answer for all of them would tell every node it was the
    /// first. A node past the end of the list is told nothing, which is what a node whose program
    /// nobody named should say.
    programs: Vec<String>,
}

impl Default for DistributedWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl DistributedWorld {
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
            template: NodeTemplate::default(),
            liveness: None,
            durability: None,
            epolls: Epolls::default(),
            // Exactly what this model narrates, and nothing it merely intends to. A catalogue that
            // advertises a fault no scenario can ask for is worse than no catalogue: a reader
            // plans around it and a campaign reports having covered it. Held to the code by
            // `scripts/lint-rules.sh`, which compares this list against every `FaultFired` the
            // crate emits — the two drifted apart once already because nothing read either.
            catalog: FaultCatalog::new(vec![
                FaultKind::NetDelay,
                FaultKind::NetDrop,
                FaultKind::NetPartition,
                FaultKind::DiskTear,
                FaultKind::DiskFull,
                FaultKind::DiskError,
                FaultKind::NodeKill,
                FaultKind::NodeFreeze,
                FaultKind::NodeThaw,
                FaultKind::NodeRestart,
                FaultKind::ChainReorg,
            ]),
        }
    }

    /// The same world, wired to a network.
    ///
    /// Scheduled partitions are armed here, before anything runs, so a scenario file replays as a
    /// pure function of its ticks.
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
    /// The path is what the guest was told through its environment; a descriptor opened on it is
    /// the record channel, and everything written there is a record rather than a file.
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
    /// Off unless a scenario asks for it, and that is what keeps every stored bundle replaying: a
    /// diagnostic's code enters the trace digest, so a watcher that spoke unconditionally would
    /// move the digest of every clustered run ever recorded.
    #[must_use]
    pub fn with_liveness(mut self, config: LivenessConfig) -> Self {
        self.liveness = Liveness::new(Some(config));
        // The first of the checks that ask which nodes have stopped. Armed here rather than at the
        // first call, so the span a scenario named is measured from the beginning of the run and a
        // node that never made a call at all is still asked about.
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

    /// Tells the liveness watch what this world just did to a node.
    ///
    /// Beside every `NodeState` this world puts on the timeline, and `scripts/lint-rules.sh` counts
    /// the two against each other. The watch cannot tell a node we stopped from a node that
    /// stopped, so a state change it is not told about becomes a finding against the world's own
    /// doing — see [`watch::Liveness::node_became`].
    fn node_became(&mut self, node: NodeId, state: NodeState, now: Tick) {
        if let Some(watch) = self.liveness.as_mut() {
            watch.node_became(node, state, now);
        }
    }

    /// What one projection came to, read off the node the scenario named.
    ///
    /// Named rather than assumed, because the node under comparison is not always the node holding
    /// the answer. An execution client is driven by a caller, and it is the caller that writes down
    /// what the client did — so the value a comparison turns on lives on a node that is not itself
    /// being compared. Reading node zero by convention found nothing there, and a projection that
    /// finds nothing is a comparison that did not happen.
    #[must_use]
    pub fn project(&self, spec: &crate::ProjectionSpec) -> Option<crate::Value> {
        self.nodes.get(&NodeId(spec.node))?.project(&spec.source)
    }

    /// The same cluster, watching for an answer the disk did not keep.
    ///
    /// Off unless a scenario asks for it, for the same reason the liveness watch is: what it
    /// records enters the timeline, and a watcher that spoke unconditionally would move the digest
    /// of every clustered run ever recorded.
    #[must_use]
    pub fn with_durability(mut self, config: DurabilityConfig) -> Self {
        self.durability = Durability::new(Some(config));
        self
    }

    /// The same cluster, surveying rather than refusing what it cannot model.
    ///
    /// Set before anything runs, because it decides what every node's world is when it is born.
    #[must_use]
    pub fn surveying(mut self) -> Self {
        self.template.surveying = true;
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
    /// Summed across nodes rather than reported per node: what a survey answers is "does this
    /// program need calls this build does not have", and five copies of one program need the same
    /// ones.
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
    /// Read once the crash has already put the surviving image in place, which is the only moment
    /// the question can be asked: before it, the node still has everything; after the guest starts
    /// again, whatever it writes next is indistinguishable from what survived.
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
    /// The moment, and not the message: this model has no word for an acknowledgement and the
    /// milestone forbids reading the guest's protocol, so what stands in for one is the instant a
    /// node reached outward with unsynced writes behind it. See `watch::Durability`.
    ///
    /// The answer goes on the timeline as a domain event, because a reader of a report has to be
    /// able to see where the node said yes — and as an event rather than a diagnostic, because a
    /// diagnostic is what the oracle judges on and this is not a finding by itself.
    fn note_an_answer(
        &mut self,
        node: NodeId,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<(), KernelError> {
        if self.durability.is_none() || !self.disks.get(&node).is_some_and(NodeDisk::is_dirty) {
            return Ok(());
        }
        // **And the node has to have written something since it last went looking for work.** A
        // dirty disk and an outgoing packet are not enough on their own: a service answers reads
        // over the socket it answers writes on, and its replies to reads are packets like any
        // other. Marking those makes a promise out of "I do not have that record", and a crash
        // then reports a broken promise nobody made — measured at forty-one runs in a hundred and
        // fifty before this condition existed.
        //
        // It errs the other way instead, and says so: a node that wrote in the background and
        // answered later is not marked, so what it had at stake goes unrecorded. A mark not made
        // costs a finding that might have been; a mark made wrongly costs every finding, because
        // nobody reads an oracle that cries on a third of its runs.
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
    /// **The one place a node is born.** A node materialised down one path and answering
    /// differently from one born down another is a difference nothing downstream would notice, and
    /// the disk is the half that is easy to forget: its baseline is the disk as the node first saw
    /// it, and capturing it any later would let an unsynced write sneak into what a crash keeps.
    ///
    /// Built on demand rather than in advance, because this is reached at every syscall and a
    /// template carrying a prepared filesystem materialises it in full.
    fn node_parts(&mut self, node: NodeId) -> (&mut MinimalWorld, &mut NodeDisk) {
        let Self { nodes, disks, template, .. } = self;
        let world = nodes.entry(node).or_insert_with(|| fresh_node(node, template));
        let disk = disks.entry(node).or_insert_with(|| NodeDisk::new(world));
        (world, disk)
    }

    fn node_world(&mut self, node: NodeId) -> &mut MinimalWorld {
        // A node that never completes a sync at all is the case the whole oracle is about, so the
        // watcher has to know the node exists before it can notice it saying nothing.
        if let Some(watch) = self.liveness.as_mut() {
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
        // Recorded before the crash takes it away, because afterwards nothing can say what was at
        // risk. Attributed explicitly: a crash arrives from a scheduled fault as often as from a
        // drawn one, and the context has no current task while deadlines are firing.
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
        let (world, disk) = self.node_parts(node);
        let image = disk.crash_image(node, world, ctx)?;
        self.nodes.insert(node, restored_node(&self.template, image));
        self.node_of_task.retain(|_, n| *n != node);
        self.net.drop_node(node, wakes);
        self.epolls.drop_node(node);
        Ok(())
    }

    /// Answers the calls the disk model has an opinion about, on descriptors that are files.
    ///
    /// Data calls are answered by the node's own world and additionally recorded as `DiskOp` —
    /// the syscall row says what the guest was told, the disk row what the disk did. `fsync` is
    /// answered here in full, because durability is this model's promise, not the file model's.
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
                // All three draws happen on every sync, whatever falls out: a drawn stall must
                // not change what the next draw costs, or state would move the tape.
                let eio = draw_odds(ctx, self.disk_config.eio_probability, DrawSite::DiskError)?;
                let stall =
                    draw_odds(ctx, self.disk_config.stall_probability, DrawSite::DiskLatency)?;
                let duration =
                    ctx.draw_exponential(self.disk_config.stall_mean_ticks, DrawSite::DiskLatency)?;

                if eio {
                    // Linux reports a failed sync once and drops the dirty pages; the unsynced
                    // data is gone, and pretending otherwise is the bug class fsyncgate named.
                    // This file's pages and no other: the failure says nothing about any file it
                    // was not asked about.
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
                // Fed where durability happens rather than where it is narrated. A stalled sync
                // emits its `DiskOp` at the start of the stall and nothing at the end, so a reader
                // of the timeline counts an interrupted sync as durability and misses the real one.
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
                let enospc =
                    draw_odds(ctx, self.disk_config.enospc_probability, DrawSite::DiskError)?;
                let offset = if matches!(nr, sys::PWRITE64 | sys::PWRITEV) {
                    args[3]
                } else {
                    self.node_world(node).file_position(fd)
                };
                // A scattering call carries its count of segments where a plain one carries its
                // length, so the length is worked out rather than read off the register.
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
                    // Which bytes of which file, and not merely that something happened: what a
                    // crash decides is whether each sector landed, and it cannot decide that
                    // about a range nobody recorded.
                    let inode = self.node_world(node).inode_of(fd);
                    if let (Some(inode), Some(disk)) = (inode, self.disks.get_mut(&node)) {
                        disk.mark_written(inode, offset, ret as u64);
                    }
                    self.wrote_since_asking.insert(node);
                    // A descriptor opened to write through is durable at the write, so the write
                    // is where it becomes durable. The failures this model draws stay with the
                    // syncs a caller asks for by name: one asked for at open, once, is not a call
                    // and has nowhere to put a drawn fate.
                    if let Some(inode) = inode {
                        if self.node_world(node).writes_through(fd) {
                            let depth = self.disk_config.snapshot_depth;
                            let (world, disk) = self.node_parts(node);
                            disk.commit(world, Some(inode), depth);
                        }
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
                // The marks above the new length go with the bytes. Left behind, one named a
                // sector the file no longer has, and the crash image read it out of a file that
                // had been cut down.
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

    /// Draws whether this node dies at this moment, where a campaign armed drawn faults.
    ///
    /// The draw happens at every syscall whatever the targeting says, and only the odds change:
    /// under `guided` they are zero except where a fault would cost the guest something. Skipping
    /// the draw instead would make byte consumption depend on the state of a disk, and then one
    /// mutated byte would shift every decision after it — which is the property the whole draw ABI
    /// is built to protect. See `docs/API.md` §5.4.
    fn maybe_kill(
        &mut self,
        node: NodeId,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Option<Effects>, KernelError> {
        if !self.fault_policy.is_armed() {
            return Ok(None);
        }
        // The moment guided targeting aims at: something was written and not yet synced, so a
        // crash here loses it. A guest that acknowledged before syncing loses an acknowledged
        // write; one that did not loses nothing, and telling those apart is the whole search.
        let candidate = self.disks.get(&node).is_some_and(NodeDisk::is_dirty);
        let odds = self.fault_policy.odds_at(candidate, ctx.world_now(), self.faults_fired);
        if !draw_odds(ctx, odds, DrawSite::NodeFault)? {
            return Ok(None);
        }
        // Counted here rather than at each kind below, because what a budget bounds is fires: a
        // fire spent on a kind the node's state does not suit is a fire the run does not get back.
        self.faults_fired = self.faults_fired.saturating_add(1);

        // Which kind this fire is. Over the whole enabled set and never over the kinds that suit
        // the node's state, because a width that depended on state is a width one mutated byte
        // moves everything after — the same rule the odds above follow. A kind that does not suit
        // the node is a fire spent, and a scenario narrows the set if it minds.
        let kinds = self.fault_policy.kinds.enabled();
        let Some(kind) =
            kinds.get(ctx.draw_index(kinds.len().max(1), DrawSite::NodeFault)?).cloned()
        else {
            return Ok(None);
        };

        // An isolation is a cut and not a lifecycle event, so it is raised through the network's
        // own partition list: the same list, and the same raising, healing and narration, that a
        // scenario writing `[[partition]]` gets. It heals after what the policy says a node comes
        // back after, and a policy that says nothing leaves the node cut off — which is what a
        // scenario asking for no recovery asked for.
        if kind == NodeFaultKind::Isolate {
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

        // The same shape as the cut above and a different fault: the links stay up and stop being
        // good. How bad is drawn, because a search that degraded every link the same way would be
        // searching one point of a class whose whole interest is that it has a range.
        if kind == NodeFaultKind::Degrade {
            let at = ctx.world_now();
            let until = Tick(at.get().saturating_add(self.fault_policy.restart_after.get()));
            let bad = &DEGRADATIONS[ctx.draw_index(DEGRADATIONS.len(), DrawSite::NetPartition)?];
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

        // A freeze goes through the list a scenario writes its own faults in, so a drawn lifecycle
        // event and a written one are the same code and cannot drift apart — which is what the
        // restart below has always done. The kill keeps the immediate path because it is the one
        // kind whose moment has to be exact: a crash that lands after the call it was drawn at is
        // a crash at a different moment, and that moment is the whole of a durability search.
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
        // A killed node takes its side of every connection with it, and the peers waiting on those
        // connections are on nodes that are still running: they are released here, reading the end
        // of a stream, which is how a guest finds out its counterpart is gone.
        let mut wakes = Vec::new();
        self.crash(node, ctx, &mut wakes)?;

        // Armed the same way a scenario's own restart is, through the timer queue and the fault
        // list, so a drawn recovery and a written one are the same code and cannot drift apart.
        if self.fault_policy.restart_after.get() > 0 {
            // The world clock, not the node's. Timers are popped against the world clock and every
            // other arming site uses it; this one read the node's own, so a node carrying a large
            // clock offset came back at a moment `restart_after` never named. The policy's own
            // documentation says "against the world clock and never a node's own".
            let at =
                Tick(ctx.world_now().get().saturating_add(self.fault_policy.restart_after.get()));
            if let Some(back) = self.fault_policy.kinds.recovery(&NodeFaultKind::Kill) {
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
    /// One place, because a drawn fault and a written one have to be applied by the same code: two
    /// applications would be two things to keep in step, and the first thing to drift would be
    /// whichever the tests do not cover.
    fn schedule_fault(&mut self, at: Tick, node: NodeId, kind: NodeFaultKind) {
        self.node_faults.push(ScheduledNodeFault { at_tick: at, node, kind });
        self.timers.arm(at, node, Due::NodeFault { index: self.node_faults.len() - 1 });
    }

    /// Arms drawn node faults for this run.
    #[must_use]
    pub fn with_fault_policy(mut self, policy: FaultPolicy) -> Self {
        self.fault_policy = policy;
        self
    }

    /// Takes a write to the record channel: the bytes are records, and the guest is told they
    /// were written, because from its side they were.
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
    /// **Readiness is a question, not a state.** Nothing here holds a notion of "ready"; it asks
    /// the network whether a socket has a datagram and treats everything else as always ready,
    /// which is what this world's files and counters are. A descriptor nobody can answer for is
    /// reported as ready, because a poller told nothing about a descriptor spins on it forever and
    /// a poller told it is ready simply reads it and finds out.
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
                // The event structure is only read where one is given: `EPOLL_CTL_DEL` is
                // documented to ignore it, and glibc has passed a null pointer there for years.
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
                    return answer(wrote as i64, ctx);
                }
                // Nothing to report and the guest is willing to wait. A timeout is armed only
                // where one was given, so a poller asked to wait forever waits forever — and a
                // run where every task does that is a deadlock, which is the truth about it.
                self.epolls.park(node, epfd, ctx.task(), out, max);
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
    /// A read empties it and answers what it held, blocking where it holds nothing — except that
    /// nothing here blocks on one: a runtime reads it only after a poller said it was ready, and
    /// answering `EAGAIN` is what a non-blocking counter does anyway.
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
        ctx.emit(EventKind::Syscall { nr, args, ret, emulated: true, data_digest: None })?;
        Ok(Effects::go(Resume::Emulate { ret }, SYSCALL_COST))
    }

    /// Hands every parked poller whatever its instance can now report.
    ///
    /// A poller is woken with its answer already in its buffer, because the call it is parked in
    /// has no second half: `epoll_wait` returns a count, and the count is what the wake carries.
    fn wake_ready_pollers(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let instances: Vec<(NodeId, i64)> = self.epolls.parked_instances();
        for (node, epfd) in instances {
            let ready = self.ready_on(node, epfd);
            if ready.is_empty() {
                continue;
            }
            if let Some((task, out, max)) = self.epolls.take_parked(node, epfd) {
                let taken = ready.len().min(max);
                let mut bytes = Vec::with_capacity(taken * epoll::EVENT_BYTES);
                for one in &ready[..taken] {
                    bytes.extend_from_slice(&one.encode());
                }
                if !bytes.is_empty() {
                    ctx.write_mem_for(task, out, &bytes)?;
                }
                wakes.push(Wake { task, ret: taken as i64 });
            }
        }
        Ok(())
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
                    // Anything else this world knows of is a file, and a file always has
                    // something to give.
                    .unwrap_or(true);
                let mut events = 0;
                if readable && wanted & epoll::EPOLLIN != 0 {
                    events |= epoll::EPOLLIN;
                }
                // Nothing here has a send buffer to fill, so a descriptor asked about for writing
                // is always ready to be written to.
                if wanted & epoll::EPOLLOUT != 0 {
                    events |= epoll::EPOLLOUT;
                }
                (events != 0).then_some(epoll::Ready { events, token })
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
    /// Every answered call is recorded as an emulated `Syscall` event, the same shape the per-node
    /// world records for what it answers.
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
                // A datagram socket addresses every send explicitly, so its connect is remembered
                // nowhere and answered yes; a stream's is the whole of how it comes to exist.
                Some((dest, port)) if self.net.is_stream(node, args[0] as i64) => {
                    let to = Endpoint { node: dest, port };
                    self.net.connect(node, args[0] as i64, to, ctx, &mut wakes)?
                }
                // **Remembered, because the next `send` will have no address of its own.**
                // Answered yes and forgotten, a connected datagram socket's `send` went looking
                // for a stream, was told `EBADF` for a descriptor this world had issued, and the
                // audit then refused the whole run -- blaming the descriptor for what `connect`
                // had failed to record. Connect-then-send is how every stub resolver works.
                Some((dest, port)) => {
                    self.net.connect_datagram(node, args[0] as i64, Endpoint { node: dest, port })
                }
                None => -errno::EINVAL,
            },
            socket_sys::SHUTDOWN => self.net.shutdown(node, args[0] as i64, &mut wakes),
            // One option is remembered and the rest are accepted and forgotten. `SO_REUSEPORT` is
            // how a server spreads one port across its threads, and a model that forgot it refused
            // the second bind of a pair the program had every right to make.
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
            // Every option this model has an opinion about reads back zero; a guest branching on
            // one it never set is branching on a value the host would have chosen.
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
                // `send` is this call with no address, which is what a connected stream uses: it
                // already knows where its bytes go, and there is nothing at the pointer to read.
                let destination = if args[4] == 0 {
                    // A datagram socket that connected has an address of its own to use; a stream
                    // has one by construction. Without this the call went looking for a stream,
                    // found a datagram socket and answered `EBADF` for a descriptor this world
                    // had issued -- which the audit then read as a defect in the tool.
                    self.net.datagram_peer(node, args[0] as i64).map(|to| (to.node, to.port))
                } else {
                    match decode_sockaddr(ctx, GuestAddr(args[4]), node)? {
                        Some(named) => Some(named),
                        None => {
                            return self.answer_socket_call(nr, args, -errno::EINVAL, wakes, ctx)
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
            // Going back for another request closes the exchange behind it: whatever the node
            // wrote belongs to the answer it has already given, not to the one it is about to.
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
                if self.net.set_nonblocking(node, fd_of(args), on) {
                    0
                } else {
                    -errno::EBADF
                }
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
                // A stream is entitled to take less than it was offered, and the count returned
                // says so: a caller with more than one call's worth is told how much landed and
                // comes back with the rest.
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
    /// A stream lands in its peer's inbox as the write happens, and a connection becomes
    /// acceptable as the connect happens; neither goes through a delivery timer, so this is the
    /// only place a poller waiting on one can be told.
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
    /// The deadline is armed in world ticks: a node-local duration and a world duration differ by
    /// at most the drift bound, and modelling that difference would let a drifted clock stretch
    /// real scheduling — the disagreement guests should see is in what they *read*, not in how
    /// long the world makes everyone wait.
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
            value.saturating_sub(MinimalWorld::presented_now(ctx).get())
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
        // A node whose program finished is silent for the rest of the run by having done its work.
        // Read off the effects rather than watched for, because ending is the one thing a node does
        // that it cannot afterwards be asked about.
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
        // Told to the network first, and unconditionally: a socket outlives the thread reading it,
        // so a task that ended while parked in a receive would otherwise be handed a delivery it
        // cannot take.
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
                Due::FutexTimeout { node, task, word } => {
                    // A timeout whose waiter was already woken fires into nothing; checking at
                    // firing time is what makes cancellation unnecessary.
                    if self.nodes.get_mut(&node).is_some_and(|w| w.cancel_wait(task, word)) {
                        effects.wake.push(Wake { task, ret: -errno::ETIMEDOUT });
                    }
                }
                Due::PollTimeout { node, epfd, task } => {
                    // A wait that ran out reports nothing, which is what an empty ready list is.
                    // The task this timer belongs to and no other: taking whoever had waited
                    // longest put an unrelated waiter back with its output buffer forgotten and
                    // left this one parked with its timer already spent. Answered only where it is
                    // still parked, because something may have woken it and waking it twice would
                    // hand the backend a task it has resumed.
                    if self.epolls.take_parked_task(node, epfd, task).is_some() {
                        effects.wake.push(Wake { task, ret: 0 });
                    }
                }
                Due::PacketDelivery { id } => {
                    self.net.deliver(id, ctx, &mut effects.wake)?;
                    // A delivery is the one thing that makes a socket readable, so it is where a
                    // poller waiting on one is told. Asked of every instance rather than tracked
                    // per descriptor: an instance watches a handful of descriptors, and a poller
                    // woken with nothing ready simply asks again.
                    self.wake_ready_pollers(ctx, &mut effects.wake)?;
                }
                Due::PartitionChange { index, raise } => {
                    self.net.set_partition(index, raise, ctx, &mut effects.wake)?;
                    // A heal carries on the streams the cut had stopped, which makes sockets
                    // readable outside any delivery timer, so the pollers are told here too.
                    self.wake_ready_pollers(ctx, &mut effects.wake)?;
                    // A window opens when the cluster can talk to itself again, which is the last
                    // cut lifting and not any cut lifting.
                    if !raise && self.net.fully_healed() {
                        if let Some(watch) = self.liveness.as_mut() {
                            let due = watch.healed(ctx.world_now());
                            self.timers.arm(due, NodeId::ZERO, Due::LivenessWindow);
                        }
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
                    // Re-armed whatever it found, because a node can go quiet after this one.
                    if let Some(span) = self.liveness.as_ref().and_then(|w| w.quiet_span()) {
                        let at = Tick(now.get().saturating_add(span.get()));
                        self.timers.arm(at, NodeId::ZERO, Due::QuietCheck);
                    }
                }
                Due::DiskStallDone { node, task } => {
                    let depth = self.disk_config.snapshot_depth;
                    let (world, disk) = self.node_parts(node);
                    // A stall whose node crashed meanwhile fires into nothing: the crash already
                    // consumed the in-flight record when it tore the file.
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
                        // Never on this list: both are raised where they are drawn, through the
                        // partition and the grey window, because that is the code that already
                        // knows how to make a link bad and put it back.
                        NodeFaultKind::Isolate | NodeFaultKind::Degrade => {
                            return Err(KernelError::Invariant(
                                "a link fault is not a listed node fault",
                            ))
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
                        // Every node at one moment, to a common ancestor, with what came after it
                        // discarded. Ordered by node so the timeline is a pure function of the
                        // scenario rather than of a map's iteration.
                        NodeFaultKind::ChainReorg { back, nodes } => {
                            let targets: Vec<NodeId> = if nodes.is_empty() {
                                let mut every: Vec<NodeId> = self.nodes.keys().copied().collect();
                                every.sort_unstable();
                                every
                            } else {
                                nodes
                            };

                            // Deliberately narrated once, at the world, and never as N node
                            // restarts: a reorg that looked like independent rewinds in the stream
                            // would be indistinguishable from N independent rewinds, and telling
                            // them apart is the whole of what this fault is for.
                            ctx.emit_for(
                                NodeId::ZERO,
                                TaskId::ZERO,
                                EventKind::FaultFired {
                                    fault: FaultKind::ChainReorg,
                                    target: FaultTarget::World,
                                },
                            )?;

                            // A node cannot be rewound while it runs: its process holds
                            // descriptors into a world that is about to be replaced. So each one
                            // dies and comes back, which is also what happens to a real node told
                            // the chain it was on is not the chain.
                            let mut applied = 0usize;
                            for node in &targets {
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

                            // How deep it actually went, which is not always how deep it was asked
                            // to: a cluster has as many sync histories as it has nodes.
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
        // A node doing anything at all is a node that has not stopped. Recorded before the stop is
        // answered, because what is being told apart is a node that polls from one that does not.
        if let Some(watch) = self.liveness.as_mut() {
            watch.stirred(node, ctx.world_now());
        }

        if let StopKind::SyscallEntry { nr, args } = stop.kind {
            if let Some(killed) = self.maybe_kill(node, ctx)? {
                return Ok(killed);
            }
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
            // **`poll` is not routed to the network, so it must not answer about it.** A socket
            // holding a queued datagram is ready; delegated to the node's world, the call was
            // answered "nothing is ready" and a poll-based server -- the older half of Linux
            // network programming -- spun or hung and was reported hung. Refused by name, which is
            // what §5.7 asks of a call this build does not model, and the message says what to use.
            if nr == sys::POLL && self.net.node_has_sockets(node) {
                return Err(ctx.refuse(
                    "BOHR-2004",
                    "poll over a socket is not modelled; this world answers the epoll family"
                        .to_owned(),
                ));
            }
            // A counter is read and written like a file and is neither: eight bytes carrying what
            // it held, and a write that adds to it. A runtime uses one to wake its own poller.
            if matches!(nr, sys::READ | sys::WRITE) && self.epolls.is_counter(node, args[0] as i64)
            {
                return self.counter_call(node, nr, args, ctx);
            }
            // A descriptor this model handed out is one the per-node world has never heard of, so
            // the questions asked about any descriptor have to be answered here too. A runtime
            // sets its poller and its counter non-blocking the moment it opens them, and a
            // `fcntl` refused there is a runtime that never starts.
            if nr == sys::FCNTL
                && (self.net.is_socket(node, args[0] as i64)
                    || self.epolls.is_counter(node, args[0] as i64)
                    || args[0] as i64 >= epoll::EPOLL_FD_BASE)
            {
                let ret = match args[1] {
                    // The descriptor flags and the file status flags. Nothing here is inherited
                    // across an exec and nothing here blocks, so both answers are the truth.
                    F_GETFD | F_SETFD | F_SETFL => 0,
                    F_GETFL => O_RDWR | O_NONBLOCK,
                    // A poller, a counter and a socket are all copied, and by whichever of the two
                    // holds the original: the descriptor spaces are disjoint, so exactly one of
                    // them can answer and the other says nothing.
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
            // read/write/close follow the descriptor: a socket's stay here, a file's go to the
            // node's own world. `writev` is among them because a server answering a request writes
            // its headers and its body in one vectored call, and a descriptor that only knew the
            // flat spelling refused the answer rather than carrying it.
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

            // A write to the record channel is a record, not a file. Recognised before the disk
            // and the network, because the descriptor is neither.
            if matches!(nr, sys::WRITE | sys::WRITEV)
                && self.sdk_fds.contains(&(node, args[0] as i64))
            {
                return self.sdk_write(node, nr, args, ctx);
            }

            // Standard output and error belong to the cluster, not to a node's filesystem: five
            // processes share one terminal, and the order they reach it is the schedule.
            // More segments than a call may carry, answered before any of them is read: taking
            // the first thousand reported a write that had left the rest behind.
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
                // Handed to the node that wrote it, because that is where a projection reads from.
                // Intercepted and not handed back, the buffer stayed empty and a differential run
                // compared nothing while reporting agreement.
                self.node_world(node).keep_written_out(&bytes);
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
            );
            if disk_shaped && self.node_world(node).is_file(args[0] as i64) {
                return self.disk_call(node, nr, args, None, stop, ctx);
            }

            // Making a mapping's writes durable is making a file durable, so it goes down the same
            // path a sync does — the same draws, the same failures, the same stall. Anything less
            // would leave a guest that only ever writes through a mapping unable to lose anything,
            // which is not a guest this engine has any business calling clean.
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

            // A timed wait is the one call whose timeout the per-node world ignores; the timeout
            // is read before delegation and armed only if the wait actually parked the task.
            let base = futex_op::base(args[1] as u32);
            let timed_wait = nr == sys::FUTEX
                && (base == futex_op::WAIT || base == futex_op::WAIT_BITSET)
                && args[3] != 0;
            if timed_wait {
                let value = MinimalWorld::read_timespec(ctx, GuestAddr(args[3]))?;
                // WAIT carries a duration, WAIT_BITSET an absolute deadline on the node's clock.
                let ahead = if base == futex_op::WAIT_BITSET {
                    value.saturating_sub(MinimalWorld::presented_now(ctx).get())
                } else {
                    value
                };
                let effects = self.node_world(node).handle(stop, ctx)?;
                if matches!(effects.state, TaskState::Blocked) {
                    let at = Tick(ctx.world_now().get().saturating_add(ahead));
                    self.timers.arm(
                        at,
                        node,
                        Due::FutexTimeout { node, task: ctx.task(), word: args[0] },
                    );
                }
                return Ok(effects);
            }
        }

        let effects = self.node_world(node).handle(stop, ctx)?;

        // An `openat` of the path the guest was told to write records to hands back a descriptor
        // that is the record channel from here on.
        if let StopKind::SyscallEntry { nr, args } = stop.kind {
            if nr == sys::OPENAT {
                if let (true, TaskState::Runnable(Resume::Emulate { ret })) =
                    (self.sdk_path.is_some(), &effects.state)
                {
                    let opened =
                        if *ret >= 0 { opened_path(ctx, GuestAddr(args[1]))? } else { None };
                    if opened.is_some() && opened == self.sdk_path {
                        self.sdk_fds.insert((node, *ret));
                    }
                }
            }
            // **A closed descriptor stops being the record channel.** Nothing removed it, and the
            // file table hands out the lowest free number: close the channel, open a data file, and
            // every write to that file was digested as SDK records -- the guest told success, the
            // file unchanged, the model's disk quietly diverging from what the guest wrote.
            if nr == sys::CLOSE {
                self.sdk_fds.remove(&(node, args[0] as i64));
            }
        }
        Ok(effects)
    }

    /// Whether this world has just told a guest it never issued a descriptor it did issue.
    ///
    /// `EBADF` is what a kernel answers for a descriptor nobody holds. A guest given it for one it
    /// is holding has been told something untrue about its own state, and everything it does
    /// afterwards is this world's doing rather than its own — which is a finding against the tool
    /// wearing the costume of a finding against the target. The class is not hypothetical: a
    /// vectored write to a socket was answered this way, and the client's behaviour afterwards was
    /// nearly filed against the client.
    ///
    /// Only calls whose first argument is a descriptor, and only where this world can say it owns
    /// one. A rule that guessed would stop runs over an argument that merely looked like a
    /// descriptor, and a false alarm here costs more than the class it would catch.
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
        let TaskState::Runnable(Resume::Emulate { ret }) = effects.state else {
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
    /// Every table, because they are one namespace from the guest's side: a file, a socket, a
    /// poller, a counter and the record channel are all numbers `close` takes.
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
/// Named one by one rather than inferred: what the audit above needs is certainty that argument
/// nought is a descriptor, and there is no rule that gives it — `openat` carries a directory
/// handle there and answers `EBADF` about the file it was asked for.
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
