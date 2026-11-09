//! Recorded facts, and the canonical projection they are digested through.
//!
//! The digest defines what "the same run" means, so its input is a hand-written byte encoding
//! rather than the serialised form: a change of serialisation library or version must not be able
//! to change what two runs compare as. Fields excluded from it are listed in `docs/API.md` §4.4.

use bohr_types::{EventId, Fd, NodeId, SyncWordId, TaskId, Tick, WorldModelId};
use serde::{Deserialize, Serialize};

/// One recorded fact.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Event {
    /// Position in the run, counted from zero.
    pub id: EventId,
    /// Virtual time at which it happened.
    pub tick: Tick,
    /// The node it happened on.
    pub node: NodeId,
    /// The task it happened on.
    pub task: TaskId,
    /// What happened.
    pub kind: EventKind,
}

/// A cluster that could talk to itself again went on committing nothing.
///
/// Raised by a world model, read by anything that reads a timeline — which is why it is spelled
/// here and not there: a reduction reconstructs what a run found from a stored log, and it depends
/// on no world. Catalogued in `docs/API.md` §8.
pub const LIVENESS_CODE: &str = "BOHR-2030";

/// A node answered while what it had written was not durable, and came back without it.
pub const DURABILITY_CODE: &str = "BOHR-2031";

/// A probe saw a value at a key change after it was seen, on one node.
pub const NEVER_REVERTS_CODE: &str = "BOHR-2040";
/// A probe saw two nodes answer differently for one key.
pub const AGREES_CODE: &str = "BOHR-2041";
/// A probe saw a value stand still for its span while the cluster was whole.
pub const ADVANCES_CODE: &str = "BOHR-2042";
/// A probe saw one key carry two values, on any node, at any time.
pub const UNIQUE_CODE: &str = "BOHR-2043";
/// A probe saw a number go down, on one node.
pub const MONOTONIC_CODE: &str = "BOHR-2044";

/// Every code a probe raises as a finding, which is what a reduction identifies such a run by.
pub const PROBE_CODES: [&str; 5] =
    [NEVER_REVERTS_CODE, AGREES_CODE, ADVANCES_CODE, UNIQUE_CODE, MONOTONIC_CODE];

/// What happened. Closed apart from [`EventKind::Domain`], which world models extend through.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum EventKind {
    /// A syscall was entered and answered.
    Syscall {
        /// Linux syscall number.
        nr: u32,
        /// Argument registers, in ABI order.
        args: [u64; 6],
        /// What the guest received.
        ret: i64,
        /// Whether the world answered it instead of the host kernel.
        emulated: bool,
        /// Digest of bytes the world wrote into the guest, truncated to eight bytes.
        ///
        /// Arguments carry addresses, not contents, so without this a syscall that delivered
        /// different data would produce an identical event and two different runs would compare
        /// as one.
        data_digest: Option<[u8; 8]>,
    },
    /// The scheduler moved execution between tasks.
    Schedule {
        /// Task that was running.
        from: TaskId,
        /// Task that runs next.
        to: TaskId,
        /// Why the switch happened.
        reason: SwitchReason,
    },
    /// A task appeared.
    TaskSpawned {
        /// The task that created it.
        parent: TaskId,
    },
    /// A task ended.
    TaskExited {
        /// How it ended.
        status: ExitStatus,
    },
    /// A packet entered the virtual network.
    PacketSent {
        /// Sender.
        from: Endpoint,
        /// Intended recipient.
        to: Endpoint,
        /// Payload length.
        len: u32,
        /// First eight bytes of the payload digest, enough to correlate without storing it twice.
        digest: [u8; 8],
    },
    /// What the network decided to do with a packet.
    PacketFate {
        /// The decision.
        fate: PacketFate,
        /// Delay applied before delivery.
        delay_ticks: u64,
    },
    /// A virtual disk operation.
    DiskOp {
        /// Descriptor it applied to.
        fd: Fd,
        /// Which operation.
        op: DiskOpKind,
        /// Byte offset within the file.
        offset: u64,
        /// Byte count.
        len: u32,
        /// What the guest received.
        ret: i64,
    },
    /// A fault was injected.
    FaultFired {
        /// Which fault.
        fault: FaultKind,
        /// What it was applied to.
        target: FaultTarget,
    },
    /// A node changed lifecycle state.
    NodeState {
        /// The new state.
        state: NodeState,
    },
    /// A task began waiting on a synchronisation word.
    Waited {
        /// The word it waits on.
        on: SyncWordId,
    },
    /// A waiting task was released, by the decision recorded here rather than by host ordering.
    Woke {
        /// The task that was released.
        task: TaskId,
        /// The word it was waiting on.
        on: SyncWordId,
    },
    /// The guest read a clock.
    ClockRead {
        /// The value it was given.
        presented: Tick,
    },
    /// An SDK assertion was evaluated.
    Assertion {
        /// Its name, as written by the guest.
        name: String,
        /// Which assertion form.
        kind: AssertionKind,
        /// Whether the site was reached.
        hit: bool,
        /// Whether the condition held.
        condition: bool,
    },
    /// An SDK record was accepted; the index points into the bundle's record section.
    SdkRecord {
        /// Record position within the section.
        index: u32,
    },
    /// Something the supervisor wants a reader to know. The message is excluded from the digest.
    ///
    /// The codes a boundary oracle raises are named below rather than in the world model that
    /// raises them: a reduction reads them back out of a stored timeline, and it cannot depend on
    /// any world. See `docs/API.md` §8.
    Diagnostic {
        /// Stable error or warning code.
        code: String,
        /// Human-readable text.
        message: String,
    },
    /// A fact the core has no vocabulary for, emitted by a world model.
    Domain {
        /// The model's namespace.
        ns: WorldModelId,
        /// The model's own event number within that namespace.
        kind: u32,
        /// Opaque to the core, which orders and digests it without interpreting it.
        payload: Vec<u8>,
    },
}

/// Why the scheduler switched tasks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SwitchReason {
    /// A syscall boundary was reached.
    SyscallBoundary,
    /// The running task can make no further progress.
    Blocked,
    /// The instruction budget ran out.
    BudgetExhausted,
    /// The task yielded.
    Yield,
    /// The task ended.
    TaskExit,
    /// An SDK fault point was reached.
    FaultPoint,
}

/// How a task ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ExitStatus {
    /// Returned a status code.
    Exited(i32),
    /// Terminated by a signal.
    Signalled(u32),
}

/// One end of a virtual network conversation.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Endpoint {
    /// The node.
    pub node: NodeId,
    /// The port, assigned by the world rather than by a host kernel.
    pub port: u16,
}

/// What the network did with a packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PacketFate {
    /// Handed to the recipient.
    Delivered,
    /// Discarded.
    Dropped,
    /// Delivered more than once.
    Duplicated,
    /// Delivered out of send order.
    Reordered,
}

/// A virtual disk operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DiskOpKind {
    /// Read bytes.
    Read,
    /// Write bytes, not necessarily durable.
    Write,
    /// Make prior writes durable.
    Fsync,
    /// Change file length.
    Truncate,
}

/// A deliberate hostility the world can apply.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FaultKind {
    /// Delay delivery.
    NetDelay,
    /// Discard a packet.
    NetDrop,
    /// Deliver a packet twice.
    NetDuplicate,
    /// Deliver out of order.
    NetReorder,
    /// Cut a link, possibly in one direction only.
    NetPartition,
    /// Write only part of a block.
    DiskTear,
    /// Reorder writes that were not yet synced.
    DiskReorder,
    /// Stall an operation.
    DiskStall,
    /// Report the volume as full.
    DiskFull,
    /// Report an I/O error.
    DiskError,
    /// Terminate a node without warning.
    NodeKill,
    /// Suspend a node.
    NodeFreeze,
    /// Resume a suspended node.
    NodeThaw,
    /// Restart a node.
    NodeRestart,
    /// Move a node's clock.
    ClockStep,
    /// Change a node's clock rate.
    ClockDrift,
    /// Rewind a set of nodes to a common older image and let history run again.
    ///
    /// Appended rather than inserted, and that is load-bearing: this enum reaches the trace digest
    /// as its declaration-order discriminant, so a variant added anywhere but the end renumbers
    /// the ones after it and changes what every stored bundle hashes to.
    ChainReorg,
}

/// What a fault was applied to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FaultTarget {
    /// A single node.
    Node(NodeId),
    /// A directed link between two nodes.
    Link {
        /// Sending end.
        from: NodeId,
        /// Receiving end.
        to: NodeId,
    },
    /// A file descriptor.
    Descriptor(Fd),
    /// The world as a whole.
    World,
}

/// Lifecycle state of a node.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum NodeState {
    /// Executing.
    Running,
    /// Suspended; time passes, the node does not run.
    Frozen,
    /// Terminated.
    Dead,
    /// Coming back up.
    Restarting,
}

/// Which SDK assertion form was evaluated.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AssertionKind {
    /// Must hold on every evaluation.
    Always,
    /// Must hold whenever reached.
    AlwaysOrUnreachable,
    /// Must hold at least once across a campaign.
    Sometimes,
    /// Must be reached at least once across a campaign.
    Reachable,
    /// Must never be reached.
    Unreachable,
}

impl Event {
    /// Feeds the canonical projection of this event into `hasher`.
    ///
    /// Host-derived values are excluded so that the same run on two machines produces the same
    /// digest. The exclusion list is a tested contract; see `docs/API.md` §4.4.
    pub fn absorb(&self, hasher: &mut blake3::Hasher) {
        hasher.update(&self.id.get().to_le_bytes());
        hasher.update(&self.tick.get().to_le_bytes());
        hasher.update(&self.node.get().to_le_bytes());
        hasher.update(&self.task.get().to_le_bytes());
        self.kind.absorb(hasher);
    }
}

/// Feeds a syscall's arguments into `hasher`, as the call itself reads them.
///
/// **A syscall is entered with six registers whatever it takes**, and the ones it does not read
/// hold whatever was there before. In a program the size of an execution client that is a pointer,
/// so hashing all six makes the digest a function of the memory layout: measured, one client under
/// two coverage map sizes produced the same 36 735 events, in the same order, with the same
/// returns, and two different digests — every one of the 11 233 differing lines an argument
/// register of a call that does not take one.
///
/// An address is hashed as whether it is null rather than as where it points. What it points *at*
/// is behaviour and is already carried: bytes the world wrote into the guest by `data_digest`, and
/// bytes the guest wrote out by whatever the world did with them. What it equals is a fact about
/// this run's memory and about nothing else.
///
/// A number the table has never heard of is hashed by all six, which is stricter than necessary and
/// never wrong — no scenario here can produce one, since an unmodelled call ends the run.
fn absorb_args(nr: u32, args: &[u64; 6], h: &mut blake3::Hasher) {
    let Some(shape) = crate::syscall_args(nr) else {
        for a in args {
            h.update(&a.to_le_bytes());
        }
        return;
    };
    for (arg, kind) in args.iter().zip(shape) {
        match kind {
            crate::Arg::Scalar => h.update(&arg.to_le_bytes()),
            crate::Arg::Address => h.update(&[u8::from(*arg != 0)]),
        };
    }
}

/// Discriminants are written explicitly rather than derived from declaration order, so that
/// inserting a variant cannot renumber the ones after it and change stored digests.
impl EventKind {
    fn absorb(&self, h: &mut blake3::Hasher) {
        match self {
            Self::Syscall { nr, args, ret, emulated, data_digest } => {
                h.update(&[1]);
                h.update(&nr.to_le_bytes());
                absorb_args(*nr, args, h);
                h.update(&ret.to_le_bytes());
                h.update(&[u8::from(*emulated)]);
                match data_digest {
                    Some(d) => {
                        h.update(&[1]);
                        h.update(d);
                    }
                    None => {
                        h.update(&[0]);
                    }
                }
            }
            Self::Schedule { from, to, reason } => {
                h.update(&[2]);
                h.update(&from.get().to_le_bytes());
                h.update(&to.get().to_le_bytes());
                h.update(&[*reason as u8]);
            }
            Self::TaskSpawned { parent } => {
                h.update(&[3]);
                h.update(&parent.get().to_le_bytes());
            }
            Self::TaskExited { status } => {
                h.update(&[4]);
                match status {
                    ExitStatus::Exited(c) => {
                        h.update(&[0]);
                        h.update(&c.to_le_bytes());
                    }
                    ExitStatus::Signalled(s) => {
                        h.update(&[1]);
                        h.update(&s.to_le_bytes());
                    }
                }
            }
            Self::PacketSent { from, to, len, digest } => {
                h.update(&[5]);
                from.absorb(h);
                to.absorb(h);
                h.update(&len.to_le_bytes());
                h.update(digest);
            }
            Self::PacketFate { fate, delay_ticks } => {
                h.update(&[6]);
                h.update(&[*fate as u8]);
                h.update(&delay_ticks.to_le_bytes());
            }
            Self::DiskOp { fd, op, offset, len, ret } => {
                h.update(&[7]);
                h.update(&fd.0.to_le_bytes());
                h.update(&[*op as u8]);
                h.update(&offset.to_le_bytes());
                h.update(&len.to_le_bytes());
                h.update(&ret.to_le_bytes());
            }
            Self::FaultFired { fault, target } => {
                h.update(&[8]);
                h.update(&[*fault as u8]);
                target.absorb(h);
            }
            Self::NodeState { state } => {
                h.update(&[9]);
                h.update(&[*state as u8]);
            }
            Self::ClockRead { presented } => {
                h.update(&[10]);
                h.update(&presented.get().to_le_bytes());
            }
            Self::Assertion { name, kind, hit, condition } => {
                h.update(&[11]);
                absorb_str(h, name);
                h.update(&[*kind as u8]);
                h.update(&[u8::from(*hit), u8::from(*condition)]);
            }
            // The word is absorbed as the ordinal it is, not as an address. Hashing where a
            // lock happened to live made the digest a function of the memory layout, which is
            // the same defect `absorb_args` was written to remove and which these two carried
            // on past it.
            Self::Waited { on } => {
                h.update(&[15]);
                h.update(&on.get().to_le_bytes());
            }
            Self::Woke { task, on } => {
                h.update(&[16]);
                h.update(&task.get().to_le_bytes());
                h.update(&on.get().to_le_bytes());
            }
            Self::SdkRecord { index } => {
                h.update(&[12]);
                h.update(&index.to_le_bytes());
            }
            // The code identifies the condition; the wording is presentation and may be reworded
            // without changing what the run did.
            Self::Diagnostic { code, message: _ } => {
                h.update(&[13]);
                absorb_str(h, code);
            }
            Self::Domain { ns, kind, payload } => {
                h.update(&[14]);
                absorb_str(h, &ns.name);
                h.update(&ns.version.to_le_bytes());
                h.update(&kind.to_le_bytes());
                h.update(&(payload.len() as u64).to_le_bytes());
                h.update(payload);
            }
        }
    }
}

impl Endpoint {
    fn absorb(self, h: &mut blake3::Hasher) {
        h.update(&self.node.get().to_le_bytes());
        h.update(&self.port.to_le_bytes());
    }
}

impl FaultTarget {
    fn absorb(&self, h: &mut blake3::Hasher) {
        match self {
            Self::Node(n) => {
                h.update(&[0]);
                h.update(&n.get().to_le_bytes());
            }
            Self::Link { from, to } => {
                h.update(&[1]);
                h.update(&from.get().to_le_bytes());
                h.update(&to.get().to_le_bytes());
            }
            Self::Descriptor(fd) => {
                h.update(&[2]);
                h.update(&fd.0.to_le_bytes());
            }
            Self::World => {
                h.update(&[3]);
            }
        }
    }
}

/// Digest of `bytes`, truncated to what an event carries.
///
/// Eight bytes is enough to make two different payloads differ in a trace while keeping the
/// contents themselves out of it: a bundle records what happened, not what the guest was told.
#[must_use]
pub fn short_digest(bytes: &[u8]) -> [u8; 8] {
    let full = blake3::hash(bytes);
    let mut out = [0u8; 8];
    out.copy_from_slice(&full.as_bytes()[..8]);
    out
}

/// Length-prefixed so that `("ab", "c")` and `("a", "bc")` cannot hash alike.
fn absorb_str(h: &mut blake3::Hasher, s: &str) {
    h.update(&(s.len() as u64).to_le_bytes());
    h.update(s.as_bytes());
}
