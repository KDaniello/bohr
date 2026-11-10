//! The virtual network: links, partitions, datagrams and byte streams.
//!
//! Normative in `docs/API.md` §5.3. Every packet makes the same four draws (drop, duplicate,
//! reorder, delay) before the partition matrix is consulted, so partitions change fates, never
//! tape layout.
//!
//! A datagram may be lost, doubled, overtaken or late. A stream makes no draws, since its
//! transport hides all four; only partitions affect it, so streams cost no tape.
//!
//! A stream carries every byte in order. Each end has a send buffer and a receive buffer; a write
//! fills the writer's send buffer, which drains into the reader's receive buffer while that has
//! room and the link carries. A writer that finds both full waits, or is told so if it may not
//! wait. Nothing is dropped: back-pressure reaches the writer as it does on Linux.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bohr_kernel::{DeadlineCtx, KernelError, StopCtx, Wake};
use bohr_tape::DrawSite;
use bohr_trace::{Endpoint, EventKind, FaultKind, FaultTarget, PacketFate};
use bohr_types::{GuestAddr, NodeId, TaskId, Tick};

use super::{FIRST_DESCRIPTOR, Numbers, draw_odds};
use crate::nums::errno;

/// First identity a socket is kept under, past every number a guest's descriptor can hold.
///
/// A guest names a socket by a descriptor number from its process's one table; the network keeps
/// the socket under an identity of its own, so a `dup` or a close of one number never moves it.
const ID_BASE: i64 = 1 << 32;

/// Datagrams a receive queue holds; an arrival past this is dropped (API §5.3).
const RECV_QUEUE_LIMIT: usize = 64;

/// Bytes each buffer of a stream holds: the send buffer at the writing end, the receive buffer at
/// the reading end.
///
/// Bounded so supervisor memory stays flat (§4.4); a writer past both is held back, as a full
/// socket buffer holds it.
const STREAM_BUFFER_LIMIT: usize = 1 << 20;

/// The `recvfrom` and `sendto` flags the model answers; Linux ignores the others it does not know,
/// and so does the model.
pub(crate) mod msg {
    /// Out-of-band data, which no stream here carries.
    pub(crate) const OOB: u64 = 0x1;
    /// Read without taking.
    pub(crate) const PEEK: u64 = 0x2;
    /// Answer a datagram's whole length, even past the buffer.
    pub(crate) const TRUNC: u64 = 0x20;
    /// Answer `EAGAIN` instead of waiting, on a blocking socket too.
    pub(crate) const DONTWAIT: u64 = 0x40;
    /// Wait until the whole buffer is filled, the stream ends, or nothing more can come.
    pub(crate) const WAITALL: u64 = 0x100;
    /// The socket's error queue, which is always empty here.
    pub(crate) const ERRQUEUE: u64 = 0x2000;
}

/// First ephemeral port, handed out upward in binding order.
const FIRST_EPHEMERAL_PORT: u16 = 32768;

/// `AF_INET`, the one address family the model speaks.
const AF_INET: u16 = 2;

/// Size of `struct sockaddr_in`.
const SOCKADDR_IN_BYTES: usize = 16;

/// The options a guest set on one socket, by level and name, as it set them.
type SocketOptions = BTreeMap<(u64, u64), Vec<u8>>;

/// How links are laid out (API §5.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Topology {
    /// Every pair is linked.
    #[default]
    Full,
    /// `i ↔ (i+1) mod n`.
    Ring,
    /// Node 0 linked to every other node.
    Star,
}

/// The network's tunables. The defaults are harmless: full topology, no loss, no delay.
#[derive(Clone, Debug)]
pub struct NetworkConfig {
    /// How links are laid out.
    pub topology: Topology,
    /// Node count, which `ring` needs to close the loop.
    pub nodes: u32,
    /// Base delivery delay.
    pub latency_mean_ticks: u64,
    /// Mean of the exponential jitter added to every delivery.
    pub latency_jitter_ticks: u64,
    /// Probability a packet is dropped, as a rational.
    pub drop_probability: (u32, u32),
    /// Probability a packet is duplicated.
    pub duplicate_probability: (u32, u32),
    /// Probability a packet is reordered.
    pub reorder_probability: (u32, u32),
    /// Probability a datagram arrives with bytes altered.
    pub corrupt_probability: (u32, u32),
    /// How many bytes an altered datagram has flipped.
    pub corrupt_bytes: u32,
    /// Scheduled directed cuts, raised and healed at their ticks.
    pub partitions: Vec<ScheduledPartition>,
    /// Scheduled grey windows: links that degrade without breaking.
    pub grey: Vec<GreyWindow>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            topology: Topology::Full,
            nodes: 0,
            latency_mean_ticks: 0,
            latency_jitter_ticks: 0,
            drop_probability: (0, 1),
            duplicate_probability: (0, 1),
            reorder_probability: (0, 1),
            corrupt_probability: (0, 1),
            corrupt_bytes: 1,
            partitions: Vec::new(),
            grey: Vec::new(),
        }
    }
}

/// A scheduled window in which one directed link degrades without breaking (API §5.3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GreyWindow {
    /// The directed link that degrades.
    pub link: (NodeId, NodeId),
    /// When the window opens.
    pub at_tick: Tick,
    /// When it closes.
    pub until_tick: Tick,
    /// Replaces the link's drop probability while open, when given.
    pub drop_probability: Option<(u32, u32)>,
    /// Multiplies the link's mean latency while open, when given.
    pub latency_factor: Option<u64>,
}

/// A scheduled partition (API §5.3): raised at one tick, healed at another.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScheduledPartition {
    /// When the cut is raised.
    pub at_tick: Tick,
    /// When it heals.
    pub heal_at_tick: Tick,
    /// Directed pairs that stop delivering, as `(from, to)`.
    pub cut: Vec<(NodeId, NodeId)>,
}

/// One undelivered packet.
#[derive(Clone, Debug)]
struct InFlight {
    from: Endpoint,
    to: Endpoint,
    payload: Vec<u8>,
    delay: u64,
    duplicated: bool,
    /// Whether bytes were altered on the wire.
    corrupted: bool,
}

/// A datagram that arrived and waits to be read.
#[derive(Clone, Debug)]
struct Arrived {
    from: Endpoint,
    payload: Vec<u8>,
}

/// A task parked in a receive, and where its answer goes.
#[derive(Clone, Copy, Debug)]
struct PendingRecv {
    task: TaskId,
    buf: GuestAddr,
    len: usize,
    /// Where the sender's address is written, when the guest asked for one. Zero for none.
    src_addr: u64,
    /// The receive's `msg` flags.
    flags: u64,
    /// Bytes already written into `buf` by a receive waiting for all of them.
    got: usize,
    /// When it parked, on the world clock; a socket timeout fires only into the wait that armed
    /// it.
    since: Tick,
}

impl PendingRecv {
    const fn peeks(&self) -> bool {
        self.flags & msg::PEEK != 0
    }

    /// Whether it waits for the whole buffer; a peek never does.
    const fn waits_for_all(&self) -> bool {
        self.flags & msg::WAITALL != 0 && !self.peeks()
    }
}

/// A task parked in a stream write until the send buffer has room for the rest of it.
#[derive(Clone, Debug)]
struct PendingWrite {
    task: TaskId,
    /// What it has yet to hand over, in order.
    rest: VecDeque<u8>,
    /// What it has handed over so far; the answer once `rest` is empty.
    written: usize,
    /// When it parked, on the world clock; a socket timeout fires only into the wait that armed
    /// it.
    since: Tick,
}

impl PendingWrite {
    /// The answer of a write cut short: what it handed over, or `EPIPE` for nothing.
    const fn cut_short(&self) -> i64 {
        if self.written > 0 { self.written as i64 } else { -errno::EPIPE }
    }
}

/// A task parked in `accept`, and where the caller's address goes.
#[derive(Clone, Copy, Debug)]
struct PendingAccept {
    task: TaskId,
    /// Where the caller's address is written; zero where the guest did not ask for one.
    addr: u64,
    /// Where the length of that address goes.
    addr_len: u64,
    /// Whether the descriptor handed out starts non-blocking, from `accept4`'s flags.
    nonblocking: bool,
    /// When it parked, on the world clock; a socket timeout fires only into the wait that armed
    /// it.
    since: Tick,
}

/// A connection made and waiting to be accepted.
#[derive(Clone, Copy, Debug)]
struct Pending {
    /// The listening node's end of it, which exists from the moment the caller connected.
    fd: i64,
    /// Who called.
    peer: Endpoint,
}

#[derive(Debug)]
enum Socket {
    /// A datagram socket; the only kind that crosses the network.
    Udp {
        owner: TaskId,
        port: Option<u16>,
        /// Where a send with no address of its own goes, once `connect` has named one.
        ///
        /// Empty until `connect`, as stub resolvers use.
        peer: Option<Endpoint>,
        queue: VecDeque<Arrived>,
        /// Tasks parked in `recvfrom`, oldest first.
        ///
        /// A queue, since servers read one socket from several threads. The longest waiter is
        /// woken.
        waiting: VecDeque<PendingRecv>,
        /// Set by `FIONBIO`: an empty queue answers `EAGAIN` instead of parking.
        nonblocking: bool,
    },
    /// One end of a byte stream: a `socketpair`, or one side of a connection.
    ///
    /// A stream makes no draws; only partitions act on it.
    Stream {
        /// The port this end answers on; zero where it was never bound.
        local: u16,
        /// The other end, absent while a connection is being made and once it is gone.
        peer: Option<(NodeId, i64)>,
        /// Where this end is connected or is trying to be. A `socketpair` has none: its two ends
        /// have no addresses at all.
        remote: Option<Endpoint>,
        /// The receive buffer: bytes that arrived and have not been read.
        inbox: VecDeque<u8>,
        /// The send buffer: bytes written and not yet in the peer's receive buffer, because it is
        /// full or the link is cut.
        outbox: VecDeque<u8>,
        /// The peer will send nothing more, so a read returns end-of-stream once `inbox` is empty.
        finished: bool,
        /// `SHUT_RD`: reads end here, and the peer is told nothing.
        read_shut: bool,
        /// `SHUT_WR`: writes fail, and the peer reads the end once the send buffer is delivered.
        write_shut: bool,
        /// The end has been delivered to the peer.
        end_sent: bool,
        waiting: VecDeque<PendingRecv>,
        /// Writers held back by full buffers, oldest first.
        writers: VecDeque<PendingWrite>,
        nonblocking: bool,
        /// For a pair of `SOCK_DGRAM` or `SOCK_SEQPACKET`, the lengths of the messages in
        /// `inbox`, oldest first: each write is one message, and each read takes one.
        messages: Option<VecDeque<usize>>,
    },
    /// A socket that accepts connections instead of carrying one.
    Listener {
        port: u16,
        /// Connections made and not yet accepted, oldest first.
        pending: VecDeque<Pending>,
        /// Tasks parked in `accept`, oldest first.
        waiting: VecDeque<PendingAccept>,
        nonblocking: bool,
    },
}

/// Which of the two independent port spaces a socket lives in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Transport {
    /// `SOCK_DGRAM`.
    Datagram,
    /// `SOCK_STREAM`, whether it is listening or connected.
    Stream,
}

/// The socket calls this part answers, whatever their descriptor.
pub(crate) const HANDLES: &[u32] = &[
    super::socket_sys::SOCKET,
    super::socket_sys::CONNECT,
    super::socket_sys::SENDTO,
    super::socket_sys::RECVFROM,
    super::socket_sys::SHUTDOWN,
    super::socket_sys::BIND,
    super::socket_sys::GETSOCKNAME,
    super::socket_sys::GETPEERNAME,
    super::socket_sys::SOCKETPAIR,
    super::socket_sys::SETSOCKOPT,
    super::socket_sys::GETSOCKOPT,
    super::socket_sys::SENDMSG,
    super::socket_sys::RECVMSG,
    super::socket_sys::LISTEN,
    super::socket_sys::ACCEPT,
    super::socket_sys::ACCEPT4,
];

/// The cluster's links, sockets and packets on the wire.
#[derive(Debug, Default)]
pub(crate) struct Network {
    config: NetworkConfig,
    sockets: BTreeMap<(NodeId, i64), Socket>,
    /// Streams closed with bytes still in their send buffer, which go before the end, as a closed
    /// socket on Linux sends what it held. No descriptor names them; each is forgotten once its
    /// end is delivered.
    lingering: BTreeMap<(NodeId, i64), Socket>,
    /// Each socket's edges, by identity, for edge-triggered watches.
    edges: BTreeMap<(NodeId, i64), super::epoll::Edges>,
    /// Options a guest set with `setsockopt`, by socket and then by level and name, as it set
    /// them.
    options: BTreeMap<(NodeId, i64), SocketOptions>,
    /// The error a socket holds for `SO_ERROR`: a connection started across a cut and refused at
    /// the heal. Read once.
    errors: BTreeMap<(NodeId, i64), i64>,
    /// Sockets made by `socketpair` or `pipe2`, which are `AF_UNIX`.
    local_pairs: BTreeSet<(NodeId, i64)>,
    /// Every descriptor a guest holds on a socket, and the identity of the socket it names.
    ///
    /// A `dup` adds a second number for one socket, sharing its queues and flags; the socket
    /// closes with its last number.
    descriptors: BTreeMap<(NodeId, i64), i64>,
    /// Which descriptors hold each port, in the order they took it.
    ///
    /// Several under `SO_REUSEPORT`.
    ports: BTreeMap<(NodeId, Transport, u16), Vec<i64>>,
    /// Descriptors whose owner asked that the port be shareable, before it was bound.
    ///
    /// Kept by descriptor, since both socket kinds carry the option.
    shareable: BTreeSet<(NodeId, i64)>,
    /// The next socket identity; never reused, so a stale reference finds nothing.
    next_id: i64,
    next_ephemeral: BTreeMap<NodeId, u16>,
    /// Directed pairs currently cut, and by how many partitions; a heal lifts only its own, so a
    /// pair two partitions cut stays cut until both heal.
    cuts: BTreeMap<(NodeId, NodeId), u32>,
    /// Open grey windows per directed link, by index into the configuration.
    grey_open: BTreeMap<(NodeId, NodeId), usize>,
    /// Packets on the wire, by the identifier their delivery timer carries.
    in_flight: BTreeMap<u64, InFlight>,
    next_packet: u64,
    /// Delivery ticks of packets on the wire per directed link, for the reorder rule.
    on_link: BTreeMap<(NodeId, NodeId), Vec<(u64, Tick)>>,
    /// The moment drawn faults stop, `faults.disarm_after_ticks`; packets sent from then on are
    /// drawn at nought odds of being dropped, duplicated, reordered or corrupted.
    window_closes: Option<Tick>,
    /// Kinds an ablation suppresses; their draws happen at nought odds.
    suppressed: super::Suppressed,
}

/// The virtual address node `n` owns: `10.66.0.(n+1)`.
fn address_of(node: NodeId) -> u32 {
    0x0A42_0000 | (node.get() + 1)
}

/// The node behind a virtual address; loopback and the wildcard both mean "this node".
///
/// A node has one address, so the wildcard means the node itself.
fn node_behind(address: u32, own: NodeId) -> Option<NodeId> {
    if address == 0x7F00_0001 || address == 0 {
        return Some(own);
    }
    if address & 0xFFFF_0000 == 0x0A42_0000 {
        let host = address & 0xFFFF;
        return (host >= 1).then(|| NodeId(host - 1));
    }
    None
}

impl Network {
    pub(crate) fn configure(&mut self, config: NetworkConfig) {
        self.config = config;
    }

    /// Stops drawn network faults from `at` on, and those of the suppressed kinds throughout.
    pub(crate) const fn follow_policy(&mut self, at: Option<Tick>, suppressed: super::Suppressed) {
        self.window_closes = at;
        self.suppressed = suppressed;
    }

    /// `odds` of `kind` for a packet sent at `now`: nought once the window has closed, and
    /// throughout where an ablation suppresses the kind.
    fn windowed(&self, now: Tick, kind: FaultKind, odds: (u32, u32)) -> (u32, u32) {
        match self.window_closes {
            Some(closes) if now >= closes => (0, 1),
            _ => self.suppressed.odds(kind, odds),
        }
    }

    fn linked(&self, a: NodeId, b: NodeId) -> bool {
        // A node always reaches itself, over loopback, whatever the topology.
        if a == b {
            return true;
        }
        let n = self.config.nodes.max(1);
        // The probe, past the cluster count, reaches every node.
        if a.get() >= n || b.get() >= n {
            return true;
        }
        match self.config.topology {
            Topology::Full => true,
            Topology::Ring => {
                let (lo, hi) = (a.get().min(b.get()), a.get().max(b.get()));
                hi - lo == 1 || (lo == 0 && hi == n - 1)
            }
            Topology::Star => a.get() == 0 || b.get() == 0,
        }
    }

    /// Opens or closes a grey window, narrating what changed on the link.
    pub(crate) fn set_grey(
        &mut self,
        index: usize,
        open: bool,
        ctx: &mut DeadlineCtx<'_, '_>,
    ) -> Result<(), KernelError> {
        let Some(window) = self.config.grey.get(index).cloned() else {
            return Err(KernelError::Invariant("a grey timer names a missing window"));
        };
        let (from, to) = window.link;
        if open {
            self.grey_open.insert(window.link, index);
        } else {
            self.grey_open.remove(&window.link);
        }
        if window.drop_probability.is_some() {
            ctx.emit_for(
                from,
                TaskId::ZERO,
                EventKind::FaultFired {
                    fault: FaultKind::NetDrop,
                    target: FaultTarget::Link { from, to },
                },
            )?;
        }
        if window.latency_factor.is_some() {
            ctx.emit_for(
                from,
                TaskId::ZERO,
                EventKind::FaultFired {
                    fault: FaultKind::NetDelay,
                    target: FaultTarget::Link { from, to },
                },
            )?;
        }
        Ok(())
    }

    /// Whether any directed link is still cut.
    ///
    /// Asked after a heal, since overlapping partitions may remain.
    pub(crate) fn fully_healed(&self) -> bool {
        self.cuts.is_empty()
    }

    /// Adds a partition the run drew, and answers its index.
    ///
    /// The same list as scheduled ones, so both share code.
    pub(crate) fn add_partition(&mut self, partition: ScheduledPartition) -> usize {
        self.config.partitions.push(partition);
        self.config.partitions.len() - 1
    }

    /// Adds a degradation the run drew, and answers its index, like [`Self::add_partition`].
    pub(crate) fn add_grey(&mut self, window: GreyWindow) -> usize {
        self.config.grey.push(window);
        self.config.grey.len() - 1
    }

    /// Every directed link between `node` and the rest of the cluster.
    ///
    /// For isolating a whole node without further draws.
    pub(crate) fn links_isolating(&self, node: NodeId) -> Vec<(NodeId, NodeId)> {
        (0..self.config.nodes)
            .map(NodeId)
            .filter(|other| *other != node)
            .flat_map(|other| [(node, other), (other, node)])
            .collect()
    }

    /// Applies or lifts a directed cut, narrating each pair.
    pub(crate) fn set_partition(
        &mut self,
        index: usize,
        raise: bool,
        numbers: &mut Numbers<'_>,
        ctx: &mut DeadlineCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let Some(partition) = self.config.partitions.get(index).cloned() else {
            return Err(KernelError::Invariant("a partition timer names a missing entry"));
        };
        for (from, to) in partition.cut {
            if raise {
                *self.cuts.entry((from, to)).or_insert(0) += 1;
            } else if let Some(held) = self.cuts.get_mut(&(from, to)) {
                *held -= 1;
                if *held == 0 {
                    self.cuts.remove(&(from, to));
                }
            }
            // The `from` node carries the event.
            ctx.emit_for(
                from,
                TaskId::ZERO,
                EventKind::FaultFired {
                    fault: FaultKind::NetPartition,
                    target: FaultTarget::Link { from, to },
                },
            )?;
        }
        if !raise {
            self.resume_streams(numbers, ctx, wakes)?;
        }
        Ok(())
    }

    /// Carries on every stream the partition had stopped.
    ///
    /// Held bytes, held ends and pending handshakes go now, in socket order; each stream keeps its
    /// own order.
    fn resume_streams(
        &mut self,
        numbers: &mut Numbers<'_>,
        ctx: &mut DeadlineCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let connecting: Vec<((NodeId, i64), Endpoint)> = self
            .sockets
            .iter()
            .filter_map(|(at, socket)| match socket {
                Socket::Stream { peer: None, remote: Some(to), finished: false, .. } => {
                    Some((*at, *to))
                }
                _ => None,
            })
            .collect();
        for (at, to) in connecting {
            if !self.reaches(at.0, to.node) {
                continue;
            }
            let port = self.local_port(at.0, at.1);
            let from = Endpoint { node: at.0, port };
            if self.complete_connect(at.0, at.1, from, to, numbers, ctx, wakes)? != 0 {
                // No listener: the started connection ends as an empty stream, and `SO_ERROR`
                // says why, as a non-blocking connect learns it.
                self.finish_peer(at, wakes);
                self.errors.insert(at, errno::ECONNREFUSED);
                self.arrived(at);
            }
        }

        let holding: Vec<(NodeId, i64)> = self
            .sockets
            .iter()
            .chain(self.lingering.iter())
            .filter_map(|(at, socket)| match socket {
                Socket::Stream { peer: Some(_), outbox, write_shut, end_sent, .. }
                    if !outbox.is_empty() || (*write_shut && !*end_sent) =>
                {
                    Some(*at)
                }
                _ => None,
            })
            .collect();
        for at in holding {
            self.pump(at, ctx, wakes)?;
        }
        Ok(())
    }

    /// Opens a datagram socket and returns its descriptor.
    pub(crate) fn open_udp(
        &mut self,
        node: NodeId,
        owner: TaskId,
        nonblocking: bool,
        numbers: &mut Numbers<'_>,
    ) -> i64 {
        let id = self.new_id();
        self.sockets.insert(
            (node, id),
            Socket::Udp {
                owner,
                port: None,
                peer: None,
                queue: VecDeque::new(),
                waiting: VecDeque::new(),
                nonblocking,
            },
        );
        self.name(node, id, numbers)
    }

    /// Whether a descriptor is a stream, which decides what a connect on it means.
    pub(crate) fn is_stream(&self, node: NodeId, fd: i64) -> bool {
        let fd = self.resolve(node, fd);
        matches!(self.sockets.get(&(node, fd)), Some(Socket::Stream { .. }))
    }

    /// Opens an unconnected stream socket.
    pub(crate) fn open_stream(
        &mut self,
        node: NodeId,
        nonblocking: bool,
        numbers: &mut Numbers<'_>,
    ) -> i64 {
        let id = self.new_id();
        self.sockets.insert((node, id), Self::stream(0, None, None, nonblocking));
        self.name(node, id, numbers)
    }

    /// Opens a connected pair of intra-node sockets, both non-blocking under `SOCK_NONBLOCK`.
    ///
    /// A pair of `SOCK_DGRAM` or `SOCK_SEQPACKET` keeps each write a message of its own; a stream
    /// pair, and a pipe, carries bytes.
    pub(crate) fn open_pair(
        &mut self,
        node: NodeId,
        nonblocking: bool,
        messages: bool,
        numbers: &mut Numbers<'_>,
    ) -> (i64, i64) {
        let a = self.new_id();
        let b = self.new_id();
        for (end, other) in [(a, b), (b, a)] {
            let mut socket = Self::stream(0, Some((node, other)), None, nonblocking);
            if messages && let Socket::Stream { messages, .. } = &mut socket {
                *messages = Some(VecDeque::new());
            }
            self.sockets.insert((node, end), socket);
            self.local_pairs.insert((node, end));
        }
        (self.name(node, a, numbers), self.name(node, b, numbers))
    }

    /// One end of a stream, in whichever of its states the caller needs.
    fn stream(
        local: u16,
        peer: Option<(NodeId, i64)>,
        remote: Option<Endpoint>,
        nonblocking: bool,
    ) -> Socket {
        Socket::Stream {
            local,
            peer,
            remote,
            inbox: VecDeque::new(),
            outbox: VecDeque::new(),
            finished: false,
            read_shut: false,
            write_shut: false,
            end_sent: false,
            waiting: VecDeque::new(),
            writers: VecDeque::new(),
            nonblocking,
            messages: None,
        }
    }

    /// The identity of the socket a descriptor names.
    ///
    /// Applied once per guest call. An identity passes through, so internal callers may hand one
    /// in; a number no socket descriptor holds answers -1, which names nothing.
    pub(crate) fn resolve(&self, node: NodeId, fd: i64) -> i64 {
        if fd >= ID_BASE {
            return fd;
        }
        self.descriptors.get(&(node, fd)).copied().unwrap_or(-1)
    }

    /// Copies a descriptor to the lowest free number at or above `at_least`, answering with it,
    /// or nothing where it is not a socket.
    pub(crate) fn dup(
        &mut self,
        node: NodeId,
        fd: i64,
        at_least: i64,
        numbers: &mut Numbers<'_>,
    ) -> Option<i64> {
        let id = self.resolve(node, fd);
        if !self.sockets.contains_key(&(node, id)) {
            return None;
        }
        let copy = numbers.claim(node, at_least);
        self.descriptors.insert((node, copy), id);
        Some(copy)
    }

    /// Copies a descriptor onto exactly `onto`, as `dup2` does; the caller has closed whatever
    /// `onto` held. False where `fd` is not a socket.
    pub(crate) fn dup_onto(
        &mut self,
        node: NodeId,
        fd: i64,
        onto: i64,
        numbers: &mut Numbers<'_>,
    ) -> bool {
        let id = self.resolve(node, fd);
        if !self.sockets.contains_key(&(node, id)) {
            return false;
        }
        numbers.claim_exact(node, onto);
        self.descriptors.insert((node, onto), id);
        true
    }

    /// A fresh socket identity.
    fn new_id(&mut self) -> i64 {
        self.next_id = self.next_id.max(ID_BASE);
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Gives a socket a descriptor number, the lowest free one in its process's table.
    fn name(&mut self, node: NodeId, id: i64, numbers: &mut Numbers<'_>) -> i64 {
        let fd = numbers.claim(node, FIRST_DESCRIPTOR);
        self.descriptors.insert((node, fd), id);
        fd
    }

    pub(crate) fn is_socket(&self, node: NodeId, fd: i64) -> bool {
        let fd = self.resolve(node, fd);
        self.sockets.contains_key(&(node, fd))
    }

    /// Whether this node holds any socket at all.
    ///
    /// Asked by the call router.
    pub(crate) fn node_has_sockets(&self, node: NodeId) -> bool {
        self.sockets.keys().any(|(held, _)| *held == node)
    }

    /// Remembers where a datagram socket's unaddressed sends go.
    pub(crate) fn connect_datagram(&mut self, node: NodeId, fd: i64, to: Endpoint) -> i64 {
        let fd = self.resolve(node, fd);
        match self.sockets.get_mut(&(node, fd)) {
            Some(Socket::Udp { peer, .. }) => {
                *peer = Some(to);
                0
            }
            Some(_) => -errno::EISCONN,
            None => -errno::EBADF,
        }
    }

    /// Where a datagram socket sends when the call names no address.
    pub(crate) fn datagram_peer(&self, node: NodeId, fd: i64) -> Option<Endpoint> {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd)) {
            Some(Socket::Udp { peer, .. }) => *peer,
            _ => None,
        }
    }

    /// Gives up one descriptor's claim on a port, leaving whatever else holds it.
    ///
    /// Only by the claiming descriptor; an accepted connection carries the port but does not hold
    /// it.
    fn release_port(&mut self, node: NodeId, transport: Transport, port: u16, fd: i64) {
        let key = (node, transport, port);
        if let Some(held) = self.ports.get_mut(&key) {
            held.retain(|other| *other != fd);
            if held.is_empty() {
                self.ports.remove(&key);
            }
        }
        self.shareable.remove(&(node, fd));
    }

    /// Which descriptor of those holding a port receives what arrives for it.
    ///
    /// One holder costs no draw; among several the tape chooses.
    fn holder(
        &mut self,
        ctx: &mut DeadlineCtx<'_, '_>,
        node: NodeId,
        transport: Transport,
        port: u16,
    ) -> Result<Option<i64>, KernelError> {
        let Some(held) = self.ports.get(&(node, transport, port)) else {
            return Ok(None);
        };
        match held.len() {
            0 => Ok(None),
            1 => Ok(held.first().copied()),
            len => {
                let which = ctx.draw_index(len, DrawSite::NetSocket)?;
                Ok(self
                    .ports
                    .get(&(node, transport, port))
                    .and_then(|held| held.get(which))
                    .copied())
            }
        }
    }

    /// Records that a descriptor's owner asked for its port to be shareable.
    pub(crate) fn share_port(&mut self, node: NodeId, fd: i64, wanted: bool) {
        let fd = self.resolve(node, fd);
        if wanted {
            self.shareable.insert((node, fd));
        } else {
            self.shareable.remove(&(node, fd));
        }
    }

    /// Binds a socket to a port, `0` meaning "hand me an ephemeral one".
    pub(crate) fn bind(&mut self, node: NodeId, fd: i64, port: u16) -> i64 {
        let fd = self.resolve(node, fd);
        let transport = match self.sockets.get(&(node, fd)) {
            Some(Socket::Udp { .. }) => Transport::Datagram,
            Some(Socket::Stream { .. }) => Transport::Stream,
            _ => return -errno::EBADF,
        };
        let requested = if port == 0 {
            let next = self.next_ephemeral.entry(node).or_insert(FIRST_EPHEMERAL_PORT);
            let handed = *next;
            *next += 1;
            handed
        } else {
            port
        };
        // A held port is shared only if every holder and the caller set `SO_REUSEPORT`, as on
        // Linux; otherwise `EADDRINUSE`.
        if let Some(held) = self.ports.get(&(node, transport, requested)) {
            let shared = self.shareable.contains(&(node, fd))
                && held.iter().all(|other| self.shareable.contains(&(node, *other)));
            if !shared {
                return -errno::EADDRINUSE;
            }
        }
        match self.sockets.get_mut(&(node, fd)) {
            Some(Socket::Udp { port, .. }) => {
                *port = Some(requested);
                self.ports.entry((node, transport, requested)).or_default().push(fd);
                0
            }
            Some(Socket::Stream { local, .. }) => {
                *local = requested;
                self.ports.entry((node, transport, requested)).or_default().push(fd);
                0
            }
            _ => -errno::EBADF,
        }
    }

    /// Turns a bound stream socket into one that accepts connections.
    ///
    /// The backlog is ignored, since there is no retransmission to stall.
    pub(crate) fn listen(&mut self, node: NodeId, fd: i64) -> i64 {
        let fd = self.resolve(node, fd);
        let Some(Socket::Stream { local, peer: None, nonblocking, .. }) =
            self.sockets.get(&(node, fd))
        else {
            return -errno::EINVAL;
        };
        let port = *local;
        // The non-blocking flag carries over.
        let nonblocking = *nonblocking;
        if port == 0 {
            return -errno::EINVAL;
        }
        self.sockets.insert(
            (node, fd),
            Socket::Listener {
                port,
                pending: VecDeque::new(),
                waiting: VecDeque::new(),
                nonblocking,
            },
        );
        0
    }

    /// Starts a connection, answering what `connect` returns.
    ///
    /// A cut link leaves the caller connecting until the heal, as a real transport would.
    pub(crate) fn connect(
        &mut self,
        node: NodeId,
        fd: i64,
        to: Endpoint,
        numbers: &mut Numbers<'_>,
        ctx: &mut StopCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<i64, KernelError> {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd)) {
            Some(Socket::Stream { peer: Some(_), .. }) => return Ok(-errno::EISCONN),
            Some(Socket::Stream { .. }) => {}
            _ => return Ok(-errno::EBADF),
        }
        let source = self.local_port(node, fd);
        if let Some(Socket::Stream { remote, .. }) = self.sockets.get_mut(&(node, fd)) {
            *remote = Some(to);
        }
        if !self.reaches(node, to.node) {
            return Ok(-errno::EINPROGRESS);
        }
        let from = Endpoint { node, port: source };
        self.complete_connect(node, fd, from, to, numbers, ctx, wakes)
    }

    /// Joins a caller to the listener it named, or refuses where nobody is listening.
    ///
    /// The accepted end exists from now, under an identity; it takes a descriptor number when it
    /// is accepted.
    #[allow(clippy::too_many_arguments)]
    fn complete_connect(
        &mut self,
        node: NodeId,
        fd: i64,
        from: Endpoint,
        to: Endpoint,
        numbers: &mut Numbers<'_>,
        ctx: &mut DeadlineCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<i64, KernelError> {
        let listener = self.holder(ctx, to.node, Transport::Stream, to.port)?;
        match listener.and_then(|l| self.sockets.get(&(to.node, l))) {
            Some(Socket::Listener { .. }) => {}
            _ => return Ok(-errno::ECONNREFUSED),
        }
        let listener = listener.unwrap_or_default();

        // Made now, since the caller may write before the accept.
        let accepted = self.new_id();
        self.sockets.insert(
            (to.node, accepted),
            Self::stream(to.port, Some((node, fd)), Some(from), false),
        );
        if let Some(Socket::Stream { peer, .. }) = self.sockets.get_mut(&(node, fd)) {
            *peer = Some((to.node, accepted));
        }
        // A connecting stream took no write; connected, it takes one, which is an edge.
        self.edges.entry((node, fd)).or_default().filled += 1;

        let Some(Socket::Listener { pending, waiting, .. }) =
            self.sockets.get_mut(&(to.node, listener))
        else {
            return Ok(-errno::ECONNREFUSED);
        };
        match waiting.pop_front() {
            Some(parked) => {
                Self::hand_over(ctx, &parked, from)?;
                if parked.nonblocking
                    && let Some(Socket::Stream { nonblocking, .. }) =
                        self.sockets.get_mut(&(to.node, accepted))
                {
                    *nonblocking = true;
                }
                let fd = self.name(to.node, accepted, numbers);
                wakes.push(Wake { task: parked.task, ret: fd });
            }
            None => pending.push_back(Pending { fd: accepted, peer: from }),
        }
        self.arrived((to.node, listener));
        // A writer that waited for the connection goes now.
        self.pump((node, fd), ctx, wakes)?;
        Ok(0)
    }

    /// Answers `accept`: with a connection already made, or by parking the task.
    ///
    /// `None` means the caller blocks.
    pub(crate) fn accept(
        &mut self,
        node: NodeId,
        fd: i64,
        want: PendingAcceptArgs,
        numbers: &mut Numbers<'_>,
        ctx: &mut StopCtx<'_, '_>,
    ) -> Result<Option<i64>, KernelError> {
        let fd = self.resolve(node, fd);
        let task = ctx.task();
        let Some(Socket::Listener { pending, waiting, nonblocking, .. }) =
            self.sockets.get_mut(&(node, fd))
        else {
            let not_listening = self.sockets.contains_key(&(node, fd));
            return Ok(Some(if not_listening { -errno::EINVAL } else { -errno::EBADF }));
        };
        let parked = PendingAccept {
            task,
            addr: want.addr,
            addr_len: want.addr_len,
            nonblocking: want.nonblocking,
            since: ctx.world_now(),
        };
        let Some(ready) = pending.pop_front() else {
            if *nonblocking {
                return Ok(Some(-errno::EAGAIN));
            }
            waiting.push_back(parked);
            return Ok(None);
        };
        Self::hand_over(ctx, &parked, ready.peer)?;
        if want.nonblocking
            && let Some(Socket::Stream { nonblocking, .. }) =
                self.sockets.get_mut(&(node, ready.fd))
        {
            *nonblocking = true;
        }
        Ok(Some(self.name(node, ready.fd, numbers)))
    }

    /// Writes the caller's address where an accept asked for it.
    fn hand_over(
        ctx: &mut DeadlineCtx<'_, '_>,
        parked: &PendingAccept,
        peer: Endpoint,
    ) -> Result<(), KernelError> {
        if parked.addr == 0 {
            return Ok(());
        }
        ctx.write_mem_for(parked.task, GuestAddr(parked.addr), &encode_sockaddr(peer))?;
        if parked.addr_len != 0 {
            ctx.write_mem_for(
                parked.task,
                GuestAddr(parked.addr_len),
                &(SOCKADDR_IN_BYTES as u32).to_le_bytes(),
            )?;
        }
        Ok(())
    }

    /// The port a stream answers on, binding it ephemerally where it has none.
    fn local_port(&mut self, node: NodeId, fd: i64) -> u16 {
        let fd = self.resolve(node, fd);
        if let Some(Socket::Stream { local, .. }) = self.sockets.get(&(node, fd))
            && *local != 0
        {
            return *local;
        }
        if self.bind(node, fd, 0) != 0 {
            return 0;
        }
        match self.sockets.get(&(node, fd)) {
            Some(Socket::Stream { local, .. }) => *local,
            _ => 0,
        }
    }

    /// Whether one node can reach another right now: the topology, then the partition matrix.
    fn reaches(&self, from: NodeId, to: NodeId) -> bool {
        from == to || (self.linked(from, to) && !self.cuts.contains_key(&(from, to)))
    }

    /// The port a socket is bound to, binding it ephemerally on first need.
    fn bound_port(&mut self, node: NodeId, fd: i64) -> Option<u16> {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd)) {
            Some(Socket::Udp { port: Some(port), .. }) => Some(*port),
            Some(Socket::Udp { .. }) => {
                let ret = self.bind(node, fd, 0);
                if ret != 0 {
                    return None;
                }
                match self.sockets.get(&(node, fd)) {
                    Some(Socket::Udp { port, .. }) => *port,
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Sends one datagram: the four draws, the matrix, and either a timer or a dropped fate.
    ///
    /// Returns what `sendto` returns to the guest.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send(
        &mut self,
        node: NodeId,
        fd: i64,
        payload: Vec<u8>,
        dest_node: NodeId,
        dest_port: u16,
        ctx: &mut StopCtx<'_, '_>,
        arm: &mut impl FnMut(Tick, NodeId, u64),
    ) -> Result<i64, KernelError> {
        let len = payload.len() as i64;
        let Some(src_port) = self.bound_port(node, fd) else {
            return Ok(-errno::EBADF);
        };
        let from = Endpoint { node, port: src_port };
        let to = Endpoint { node: dest_node, port: dest_port };

        ctx.emit(EventKind::PacketSent {
            from,
            to,
            len: payload.len() as u32,
            digest: bohr_trace::short_digest(&payload),
        })?;

        // A grey window overrides what the draws are compared against, never what is drawn.
        let (drop_probability, mean) = match self.grey_open.get(&(node, dest_node)) {
            Some(index) => {
                let window = &self.config.grey[*index];
                (
                    window.drop_probability.unwrap_or(self.config.drop_probability),
                    self.config
                        .latency_mean_ticks
                        .saturating_mul(window.latency_factor.unwrap_or(1)),
                )
            }
            None => (self.config.drop_probability, self.config.latency_mean_ticks),
        };

        // The four draws, always, in order (API §5.3), at fixed width.
        let now = ctx.world_now();
        let drop_odds = self.windowed(now, FaultKind::NetDrop, drop_probability);
        let duplicate_odds =
            self.windowed(now, FaultKind::NetDuplicate, self.config.duplicate_probability);
        let reorder_odds =
            self.windowed(now, FaultKind::NetReorder, self.config.reorder_probability);
        let dropped = draw_odds(ctx, drop_odds, DrawSite::NetDeliver)?;
        let duplicated = draw_odds(ctx, duplicate_odds, DrawSite::NetDeliver)?;
        let reordered = draw_odds(ctx, reorder_odds, DrawSite::NetReorder)?;
        let jitter = ctx.draw_exponential(self.config.latency_jitter_ticks, DrawSite::NetDelay)?;
        let delay = mean.saturating_add(jitter);
        // Corruption draws only when armed, and then always at full width.
        let flips = if self.config.corrupt_probability.0 > 0 {
            let corrupt_odds =
                self.windowed(now, FaultKind::NetCorrupt, self.config.corrupt_probability);
            let corrupted = draw_odds(ctx, corrupt_odds, DrawSite::NetCorrupt)?;
            let mut flips = Vec::with_capacity(self.config.corrupt_bytes as usize);
            for _ in 0..self.config.corrupt_bytes {
                let mut drawn = [0u8; 8];
                ctx.draw_into(&mut drawn, DrawSite::NetCorrupt);
                flips.push(u64::from_le_bytes(drawn));
            }
            if corrupted && !payload.is_empty() { flips } else { Vec::new() }
        } else {
            Vec::new()
        };

        let unreachable = !self.linked(node, dest_node);
        let cut = self.cuts.contains_key(&(node, dest_node));
        if dropped || unreachable || cut {
            ctx.emit(EventKind::PacketFate { fate: PacketFate::Dropped, delay_ticks: delay })?;
            // A drop lands on the guest as a fault would; duplicated, reordered or altered packets
            // fire their own fault and count there.
            ctx.landed();
            return Ok(len);
        }

        let now = ctx.world_now();
        let natural = Tick(now.get().saturating_add(delay).max(now.get() + 1));
        // Reordered before the earliest packet in flight on the link, if any.
        let earliest = reordered
            .then(|| {
                self.on_link
                    .get(&(node, dest_node))
                    .and_then(|flights| flights.iter().map(|(_, at)| *at).min())
            })
            .flatten();
        // Only when that moves it first; otherwise an ordinary delivery.
        let overtakes = earliest.map(|e| (e, Tick(e.get().saturating_sub(1).max(now.get() + 1))));
        let at = match overtakes {
            Some((e, slot)) if slot < e && slot < natural => slot,
            _ => natural,
        };
        let link = FaultTarget::Link { from: node, to: dest_node };
        if at < natural {
            ctx.emit(EventKind::FaultFired { fault: FaultKind::NetReorder, target: link })?;
        }
        if duplicated {
            ctx.emit(EventKind::FaultFired { fault: FaultKind::NetDuplicate, target: link })?;
        }

        // Copied only for a duplicate; corruption applies to one delivery.
        let copy = duplicated.then(|| payload.clone());
        let mut payload = payload;
        let corrupted = !flips.is_empty();
        // Each flip takes a new byte and a non-zero mask, so flips never cancel.
        let mut taken = BTreeSet::new();
        for flip in flips {
            let at = (flip % payload.len() as u64) as usize;
            if taken.insert(at) {
                payload[at] ^= ((flip >> 32) as u8) | 1;
            }
        }
        if corrupted {
            ctx.emit(EventKind::FaultFired { fault: FaultKind::NetCorrupt, target: link })?;
        }
        self.enqueue(
            node,
            dest_node,
            InFlight { from, to, payload, delay, duplicated: false, corrupted },
            at,
            arm,
        );
        if let Some(payload) = copy {
            self.enqueue(
                node,
                dest_node,
                InFlight { from, to, payload, delay, duplicated: true, corrupted: false },
                natural,
                arm,
            );
        }
        Ok(len)
    }

    fn enqueue(
        &mut self,
        from: NodeId,
        to: NodeId,
        packet: InFlight,
        at: Tick,
        arm: &mut impl FnMut(Tick, NodeId, u64),
    ) {
        let id = self.next_packet;
        self.next_packet += 1;
        self.in_flight.insert(id, packet);
        self.on_link.entry((from, to)).or_default().push((id, at));
        arm(at, to, id);
    }

    /// Delivers a packet whose timer fired: into a parked receive, or into the queue.
    pub(crate) fn deliver(
        &mut self,
        id: u64,
        ctx: &mut DeadlineCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let Some(packet) = self.in_flight.remove(&id) else {
            return Err(KernelError::Invariant("a delivery timer names a missing packet"));
        };
        if let Some(flights) = self.on_link.get_mut(&(packet.from.node, packet.to.node)) {
            flights.retain(|(flying, _)| *flying != id);
            if flights.is_empty() {
                self.on_link.remove(&(packet.from.node, packet.to.node));
            }
        }

        let dest = packet.to.node;
        let fate = if packet.corrupted {
            PacketFate::Corrupted
        } else if packet.duplicated {
            PacketFate::Duplicated
        } else {
            PacketFate::Delivered
        };
        let Some(fd) = self.holder(ctx, dest, Transport::Datagram, packet.to.port)? else {
            // Nobody listens on the port: the packet evaporates, which is what UDP promises.
            ctx.emit_for(
                dest,
                TaskId::ZERO,
                EventKind::PacketFate { fate: PacketFate::Dropped, delay_ticks: packet.delay },
            )?;
            ctx.landed();
            return Ok(());
        };
        let Some(Socket::Udp { owner, queue, waiting, .. }) = self.sockets.get_mut(&(dest, fd))
        else {
            return Err(KernelError::Invariant("a bound port names a missing socket"));
        };
        let owner = *owner;

        // Parked receives take it in order: one that peeks is handed a copy and the datagram
        // stays for the next; the first that takes it ends the search.
        let arrived = Arrived { from: packet.from, payload: packet.payload };
        let mut fated = false;
        while let Some(recv) = waiting.pop_front() {
            let answer = Self::hand_datagram(ctx, &recv, &arrived)?;
            if !fated {
                ctx.emit_for(
                    dest,
                    recv.task,
                    EventKind::PacketFate { fate, delay_ticks: packet.delay },
                )?;
                fated = true;
            }
            wakes.push(Wake { task: recv.task, ret: answer });
            if !recv.peeks() {
                return Ok(());
            }
        }

        if queue.len() >= RECV_QUEUE_LIMIT {
            ctx.emit_for(
                dest,
                owner,
                EventKind::PacketFate { fate: PacketFate::Dropped, delay_ticks: packet.delay },
            )?;
            ctx.landed();
            return Ok(());
        }
        queue.push_back(arrived);
        self.edges.entry((dest, fd)).or_default().arrived += 1;
        if !fated {
            ctx.emit_for(dest, owner, EventKind::PacketFate { fate, delay_ticks: packet.delay })?;
        }
        Ok(())
    }

    /// Answers `recvfrom`: immediately from the queue, or by parking the task.
    ///
    /// `None` means the caller blocks.
    pub(crate) fn recv(
        &mut self,
        node: NodeId,
        fd: i64,
        recv: PendingRecvArgs,
        ctx: &mut StopCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<Option<i64>, KernelError> {
        let fd = self.resolve(node, fd);
        let task = ctx.task();
        // An error queue is always empty here, and no stream carries urgent data.
        if recv.flags & msg::ERRQUEUE != 0 {
            return Ok(Some(-errno::EAGAIN));
        }
        let mut parked = PendingRecv {
            task,
            buf: recv.buf,
            len: recv.len,
            src_addr: recv.src_addr,
            flags: recv.flags,
            got: 0,
            since: ctx.world_now(),
        };
        let (may_wait, writer) = match self.sockets.get_mut(&(node, fd)) {
            Some(Socket::Udp { queue, waiting, nonblocking, .. }) => {
                let Some(arrived) = queue.front() else {
                    if *nonblocking || recv.flags & msg::DONTWAIT != 0 {
                        return Ok(Some(-errno::EAGAIN));
                    }
                    waiting.push_back(parked);
                    return Ok(None);
                };
                let answer = Self::hand_datagram(ctx, &parked, arrived)?;
                if !parked.peeks() {
                    queue.pop_front();
                }
                return Ok(Some(answer));
            }
            Some(Socket::Stream {
                inbox,
                waiting,
                finished,
                read_shut,
                nonblocking,
                peer,
                messages,
                ..
            }) => {
                if recv.flags & msg::OOB != 0 {
                    return Ok(Some(-errno::EINVAL));
                }
                let may_wait = !*nonblocking && recv.flags & msg::DONTWAIT == 0;
                let ended = *finished || *read_shut;
                if inbox.is_empty() {
                    if ended {
                        return Ok(Some(0));
                    }
                    if !may_wait {
                        return Ok(Some(-errno::EAGAIN));
                    }
                    waiting.push_back(parked);
                    return Ok(None);
                }
                if let Some(messages) = messages {
                    let answer = Self::take_message(inbox, messages, &parked, ctx)?.unwrap_or(0);
                    if let Some(writer) = *peer
                        && !parked.peeks()
                    {
                        self.pump(writer, ctx, wakes)?;
                    }
                    return Ok(Some(answer));
                }
                if parked.peeks() {
                    let taken = inbox.len().min(recv.len);
                    let bytes: Vec<u8> = inbox.range(..taken).copied().collect();
                    ctx.write_mem(recv.buf, &bytes)?;
                    return Ok(Some(taken as i64));
                }
                (may_wait, *peer)
            }
            Some(Socket::Listener { .. }) => return Ok(Some(-errno::ENOTCONN)),
            None => return Ok(Some(-errno::EBADF)),
        };

        // Take what is there; each take makes room for the other end's held bytes, which may
        // arrive at once. A receive waiting for all of it goes on taking until it has all or
        // nothing more is there, and only then waits for the rest.
        while let Some(Socket::Stream { inbox, .. }) = self.sockets.get_mut(&(node, fd)) {
            let taken = inbox.len().min(recv.len - parked.got);
            if taken == 0 {
                break;
            }
            let bytes: Vec<u8> = inbox.drain(..taken).collect();
            ctx.write_mem(GuestAddr(recv.buf.0 + parked.got as u64), &bytes)?;
            parked.got += taken;
            if let Some(writer) = writer {
                self.pump(writer, ctx, wakes)?;
            }
            if !parked.waits_for_all() || parked.got == recv.len {
                break;
            }
        }
        if parked.waits_for_all()
            && parked.got < recv.len
            && may_wait
            && let Some(Socket::Stream { waiting, finished, read_shut, .. }) =
                self.sockets.get_mut(&(node, fd))
            && !*finished
            && !*read_shut
        {
            waiting.push_back(parked);
            return Ok(None);
        }
        Ok(Some(parked.got as i64))
    }

    /// Writes a datagram into a receive's buffer and answers its length: what fitted, or the
    /// whole datagram's under `MSG_TRUNC`.
    fn hand_datagram(
        ctx: &mut DeadlineCtx<'_, '_>,
        recv: &PendingRecv,
        arrived: &Arrived,
    ) -> Result<i64, KernelError> {
        let taken = arrived.payload.len().min(recv.len);
        ctx.write_mem_for(recv.task, recv.buf, &arrived.payload[..taken])?;
        if recv.src_addr != 0 {
            let addr = encode_sockaddr(arrived.from);
            ctx.write_mem_for(recv.task, GuestAddr(recv.src_addr), &addr)?;
        }
        let whole = recv.flags & msg::TRUNC != 0;
        Ok(if whole { arrived.payload.len() } else { taken } as i64)
    }

    /// Notes that something arrived at a socket: bytes, a message, a datagram, a connection, an
    /// end or an error.
    fn arrived(&mut self, at: (NodeId, i64)) {
        self.edges.entry(at).or_default().arrived += 1;
    }

    /// A socket's edges, or nothing where the descriptor is not a socket.
    pub(crate) fn edges(&self, node: NodeId, fd: i64) -> Option<super::epoll::Edges> {
        let at = (node, self.resolve(node, fd));
        self.sockets.contains_key(&at).then(|| self.edges.get(&at).copied().unwrap_or_default())
    }

    /// Writes into a stream: what fits in the send buffer at once, the rest when room appears.
    ///
    /// `None` means the writer waits for room. One that may not wait is answered what fitted, or
    /// `EAGAIN` for nothing. Under `may_wait` false, as `MSG_DONTWAIT` asks, a blocking socket
    /// answers as a non-blocking one.
    ///
    /// A write that does not go whole found the stream full, which the next room answers as an
    /// edge.
    pub(crate) fn stream_write(
        &mut self,
        node: NodeId,
        fd: i64,
        payload: &[u8],
        may_wait: bool,
        ctx: &mut StopCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<Option<i64>, KernelError> {
        let at = (node, self.resolve(node, fd));
        let done = self.write_stream(node, fd, payload, may_wait, ctx, wakes)?;
        let short = match done {
            None => true,
            Some(n) => n == -errno::EAGAIN || (0..payload.len() as i64).contains(&n),
        };
        if short {
            self.edges.entry(at).or_default().filled += 1;
        }
        Ok(done)
    }

    /// [`Self::stream_write`], without the edge it leaves.
    fn write_stream(
        &mut self,
        node: NodeId,
        fd: i64,
        payload: &[u8],
        may_wait: bool,
        ctx: &mut StopCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<Option<i64>, KernelError> {
        let at = (node, self.resolve(node, fd));
        let task = ctx.task();
        let Some(Socket::Stream {
            peer, remote, finished, write_shut, nonblocking, writers, ..
        }) = self.sockets.get_mut(&at)
        else {
            return Ok(Some(-errno::EBADF));
        };
        let may_wait = may_wait && !*nonblocking;
        // Shut for writing, ended by the other side, or never connected.
        if *write_shut || (peer.is_none() && (*finished || remote.is_none())) {
            return Ok(Some(-errno::EPIPE));
        }
        // Still connecting, or behind writers already waiting: nothing goes before them.
        if peer.is_none() || !writers.is_empty() {
            if !may_wait {
                return Ok(Some(-errno::EAGAIN));
            }
            writers.push_back(PendingWrite {
                task,
                rest: payload.iter().copied().collect(),
                written: 0,
                since: ctx.world_now(),
            });
            return Ok(None);
        }
        if let Some(Socket::Stream { messages: Some(_), .. }) = self.sockets.get(&at) {
            return self.message_write(at, payload, may_wait, task, ctx, wakes);
        }

        let mut written = 0;
        while let Some(Socket::Stream { outbox, .. }) = self.sockets.get_mut(&at) {
            let take =
                STREAM_BUFFER_LIMIT.saturating_sub(outbox.len()).min(payload.len() - written);
            outbox.extend(payload[written..written + take].iter().copied());
            written += take;
            let held = outbox.len();
            self.pump(at, ctx, wakes)?;
            // Done, or the send buffer did not drain: nothing more fits now.
            if written == payload.len() || self.held(at) == held {
                break;
            }
        }
        if written == payload.len() {
            return Ok(Some(written as i64));
        }
        if !may_wait {
            return Ok(Some(if written == 0 { -errno::EAGAIN } else { written as i64 }));
        }
        if let Some(Socket::Stream { writers, .. }) = self.sockets.get_mut(&at) {
            writers.push_back(PendingWrite {
                task,
                rest: payload[written..].iter().copied().collect(),
                written,
                since: ctx.world_now(),
            });
        }
        Ok(None)
    }

    /// Writes one message of a message pair: whole into the peer's receive buffer, or not yet.
    ///
    /// A message larger than a buffer can ever hold is `EMSGSIZE`; one that does not fit now
    /// waits, or is `EAGAIN` for a writer that may not wait.
    #[allow(clippy::too_many_arguments)]
    fn message_write(
        &mut self,
        at: (NodeId, i64),
        payload: &[u8],
        may_wait: bool,
        task: TaskId,
        ctx: &mut StopCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<Option<i64>, KernelError> {
        if payload.len() > STREAM_BUFFER_LIMIT {
            return Ok(Some(-errno::EMSGSIZE));
        }
        let Some(Socket::Stream { peer: Some(peer), .. }) = self.sockets.get(&at) else {
            return Ok(Some(-errno::EPIPE));
        };
        let peer = *peer;
        if self.inbox_room(peer) >= payload.len() {
            Self::land_message(&mut self.sockets, peer, payload, ctx, wakes)?;
            self.arrived(peer);
            return Ok(Some(payload.len() as i64));
        }
        if !may_wait {
            return Ok(Some(-errno::EAGAIN));
        }
        if let Some(Socket::Stream { writers, .. }) = self.sockets.get_mut(&at) {
            writers.push_back(PendingWrite {
                task,
                rest: payload.iter().copied().collect(),
                written: 0,
                since: ctx.world_now(),
            });
        }
        Ok(None)
    }

    /// Room in a stream's receive buffer; an end that is gone takes anything and keeps nothing.
    fn inbox_room(&self, at: (NodeId, i64)) -> usize {
        match self.sockets.get(&at) {
            Some(Socket::Stream { inbox, .. }) => STREAM_BUFFER_LIMIT.saturating_sub(inbox.len()),
            _ => usize::MAX,
        }
    }

    /// Lands the waiting writers of a message pair, each message whole, while they fit.
    fn pump_messages(
        &mut self,
        at: (NodeId, i64),
        ctx: &mut DeadlineCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        loop {
            let Some(Socket::Stream { peer: Some(peer), writers, .. }) = self.sockets.get(&at)
            else {
                return Ok(());
            };
            let peer = *peer;
            let Some(next) = writers.front().map(|writer| writer.rest.len()) else {
                return Ok(());
            };
            if next > self.inbox_room(peer) {
                return Ok(());
            }
            let Some(Socket::Stream { writers, .. }) = self.sockets.get_mut(&at) else {
                return Ok(());
            };
            let Some(writer) = writers.pop_front() else { return Ok(()) };
            let message: Vec<u8> = writer.rest.into_iter().collect();
            Self::land_message(&mut self.sockets, peer, &message, ctx, wakes)?;
            self.arrived(peer);
            wakes.push(Wake { task: writer.task, ret: message.len() as i64 });
        }
    }

    /// Puts one message into a message pair's receive buffer and hands messages to the receives
    /// waiting there, one each.
    fn land_message(
        sockets: &mut BTreeMap<(NodeId, i64), Socket>,
        at: (NodeId, i64),
        payload: &[u8],
        ctx: &mut DeadlineCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let Some(Socket::Stream { inbox, waiting, messages: Some(messages), .. }) =
            sockets.get_mut(&at)
        else {
            return Ok(());
        };
        inbox.extend(payload.iter().copied());
        messages.push_back(payload.len());
        while let Some(recv) = waiting.pop_front() {
            let Some(answer) = Self::take_message(inbox, messages, &recv, ctx)? else {
                waiting.push_front(recv);
                break;
            };
            wakes.push(Wake { task: recv.task, ret: answer });
        }
        Ok(())
    }

    /// Hands the oldest message to a receive and answers its length, or nothing where none is
    /// buffered.
    ///
    /// What does not fit the receive's buffer is lost with the message, as Linux loses it;
    /// `MSG_TRUNC` answers the whole length. A peek leaves the message buffered.
    fn take_message(
        inbox: &mut VecDeque<u8>,
        messages: &mut VecDeque<usize>,
        recv: &PendingRecv,
        ctx: &mut DeadlineCtx<'_, '_>,
    ) -> Result<Option<i64>, KernelError> {
        let Some(&length) = messages.front() else {
            return Ok(None);
        };
        let taken = length.min(recv.len);
        let bytes: Vec<u8> = inbox.range(..taken).copied().collect();
        ctx.write_mem_for(recv.task, recv.buf, &bytes)?;
        if !recv.peeks() {
            inbox.drain(..length);
            messages.pop_front();
        }
        let whole = recv.flags & msg::TRUNC != 0;
        Ok(Some(if whole { length } else { taken } as i64))
    }

    /// Bytes in a stream's send buffer.
    fn held(&self, at: (NodeId, i64)) -> usize {
        match self.sockets.get(&at).or_else(|| self.lingering.get(&at)) {
            Some(Socket::Stream { outbox, .. }) => outbox.len(),
            _ => 0,
        }
    }

    /// The stream at `at`, open or lingering after its close.
    fn stream_mut(&mut self, at: (NodeId, i64)) -> Option<&mut Socket> {
        if self.sockets.contains_key(&at) {
            self.sockets.get_mut(&at)
        } else {
            self.lingering.get_mut(&at)
        }
    }

    /// Moves what one end holds toward the other.
    ///
    /// The send buffer drains into the peer's receive buffer while that has room and the link
    /// carries, and waiting writers refill it; a writer is released once all it offered is
    /// buffered. Each move may let a reader take bytes and free room, so it repeats until nothing
    /// moves. A stream shut for writing delivers its end once nothing is held.
    fn pump(
        &mut self,
        at: (NodeId, i64),
        ctx: &mut DeadlineCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        if let Some(Socket::Stream { messages: Some(_), .. }) = self.sockets.get(&at) {
            return self.pump_messages(at, ctx, wakes);
        }
        loop {
            let Some(Socket::Stream { peer, .. }) = self.stream_mut(at) else {
                return Ok(());
            };
            let Some(peer) = *peer else {
                return Ok(());
            };
            let mut moved = false;
            if self.reaches(at.0, peer.0) {
                // An end that is closed takes nothing, and what is sent to it is lost.
                let room = match self.sockets.get(&peer) {
                    Some(Socket::Stream { inbox, .. }) => {
                        STREAM_BUFFER_LIMIT.saturating_sub(inbox.len())
                    }
                    _ => usize::MAX,
                };
                let bytes: Vec<u8> = match self.stream_mut(at) {
                    Some(Socket::Stream { outbox, .. }) => {
                        let take = room.min(outbox.len());
                        outbox.drain(..take).collect()
                    }
                    _ => Vec::new(),
                };
                if !bytes.is_empty() {
                    Self::land(&mut self.sockets, peer, &bytes, ctx, wakes)?;
                    self.arrived(peer);
                    moved = true;
                }
            }
            if let Some(Socket::Stream { outbox, writers, .. }) = self.stream_mut(at) {
                while let Some(writer) = writers.front_mut() {
                    let take =
                        STREAM_BUFFER_LIMIT.saturating_sub(outbox.len()).min(writer.rest.len());
                    if take == 0 {
                        break;
                    }
                    outbox.extend(writer.rest.drain(..take));
                    writer.written += take;
                    moved = true;
                    if !writer.rest.is_empty() {
                        break;
                    }
                    let Some(done) = writers.pop_front() else { break };
                    wakes.push(Wake { task: done.task, ret: done.written as i64 });
                }
            }
            if !moved {
                break;
            }
        }
        self.send_end_when_drained(at, wakes);
        Ok(())
    }

    /// Delivers a stream's end once it is shut for writing, holds nothing and the link carries.
    ///
    /// A shutdown leaves the other direction open; a lingering close ends the stream and is
    /// forgotten.
    fn send_end_when_drained(&mut self, at: (NodeId, i64), wakes: &mut Vec<Wake>) {
        let Some(Socket::Stream { peer, outbox, writers, write_shut, end_sent, .. }) =
            self.stream_mut(at)
        else {
            return;
        };
        if !*write_shut || *end_sent || !outbox.is_empty() || !writers.is_empty() {
            return;
        }
        let peer = *peer;
        if peer.is_some_and(|peer| !self.reaches(at.0, peer.0)) {
            return;
        }
        if let Some(Socket::Stream { end_sent, .. }) = self.stream_mut(at) {
            *end_sent = true;
        }
        let closed = self.lingering.remove(&at).is_some();
        if let Some(peer) = peer {
            if closed {
                self.finish_peer(peer, wakes);
            } else {
                self.half_finish(peer, wakes);
            }
        }
    }

    /// Puts bytes into a stream's receive buffer and hands them to the receives waiting there.
    ///
    /// The caller brings no more than the buffer has room for. A receive waiting for all of its
    /// buffer takes what came and waits on; one that peeks is handed a copy.
    fn land(
        sockets: &mut BTreeMap<(NodeId, i64), Socket>,
        at: (NodeId, i64),
        payload: &[u8],
        ctx: &mut DeadlineCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let Some(Socket::Stream { inbox, waiting, .. }) = sockets.get_mut(&at) else {
            return Ok(());
        };
        inbox.extend(payload.iter().copied());
        while let Some(recv) = waiting.front_mut() {
            if inbox.is_empty() {
                break;
            }
            let taken = inbox.len().min(recv.len - recv.got);
            let into = GuestAddr(recv.buf.0 + recv.got as u64);
            if recv.peeks() {
                let bytes: Vec<u8> = inbox.range(..taken).copied().collect();
                ctx.write_mem_for(recv.task, into, &bytes)?;
                wakes.push(Wake { task: recv.task, ret: taken as i64 });
                waiting.pop_front();
                continue;
            }
            let bytes: Vec<u8> = inbox.drain(..taken).collect();
            ctx.write_mem_for(recv.task, into, &bytes)?;
            recv.got += taken;
            if recv.waits_for_all() && recv.got < recv.len {
                break;
            }
            wakes.push(Wake { task: recv.task, ret: recv.got as i64 });
            waiting.pop_front();
        }
        Ok(())
    }

    /// Releases every receive waiting on a stream that will get nothing more, with what each
    /// already took.
    fn release_readers(waiting: &mut VecDeque<PendingRecv>, wakes: &mut Vec<Wake>) {
        for recv in waiting.drain(..) {
            wakes.push(Wake { task: recv.task, ret: recv.got as i64 });
        }
    }

    /// Tells a stream that its peer sends nothing more, while the peer still reads.
    ///
    /// The reader takes what is buffered, then reads the end; its own writes still go.
    fn half_finish(&mut self, at: (NodeId, i64), wakes: &mut Vec<Wake>) {
        if let Some(Socket::Stream { finished, waiting, .. }) = self.sockets.get_mut(&at) {
            *finished = true;
            Self::release_readers(waiting, wakes);
            self.arrived(at);
        }
    }

    /// Tells a stream its peer is gone, releasing any reader and writer parked on it.
    ///
    /// How a guest learns its peer is gone. Bytes this end still held for the peer have nowhere
    /// to go and are dropped; a waiting writer is answered what it handed over, or `EPIPE`.
    fn finish_peer(&mut self, peer: (NodeId, i64), wakes: &mut Vec<Wake>) {
        if self.lingering.remove(&peer).is_some() {
            return;
        }
        let Some(Socket::Stream { finished, peer: theirs, waiting, writers, outbox, .. }) =
            self.sockets.get_mut(&peer)
        else {
            return;
        };
        *finished = true;
        *theirs = None;
        outbox.clear();
        Self::release_readers(waiting, wakes);
        for writer in writers.drain(..) {
            wakes.push(Wake { task: writer.task, ret: writer.cut_short() });
        }
        self.arrived(peer);
    }

    /// Forgets everything a node's process held: its sockets and its ports. Packets already
    /// flying toward it will find nobody listening and evaporate, which is what UDP promises.
    ///
    /// Every stream it carried ends, and its peers are told.
    pub(crate) fn drop_node(&mut self, node: NodeId, wakes: &mut Vec<Wake>) {
        let bereaved: Vec<(NodeId, i64)> = self
            .sockets
            .iter()
            .chain(self.lingering.iter())
            .filter(|((owner, _), _)| *owner == node)
            .filter_map(|(_, socket)| match socket {
                Socket::Stream { peer: Some(peer), .. } if peer.0 != node => Some(*peer),
                _ => None,
            })
            .collect();
        self.sockets.retain(|(owner, _), _| *owner != node);
        self.lingering.retain(|(owner, _), _| *owner != node);
        self.ports.retain(|(owner, _, _), _| *owner != node);
        self.shareable.retain(|(owner, _)| *owner != node);
        self.descriptors.retain(|(owner, _), _| *owner != node);
        self.options.retain(|(owner, _), _| *owner != node);
        self.errors.retain(|(owner, _), _| *owner != node);
        self.edges.retain(|(owner, _), _| *owner != node);
        self.local_pairs.retain(|(owner, _)| *owner != node);
        self.next_ephemeral.remove(&node);
        for peer in bereaved {
            self.finish_peer(peer, wakes);
        }
    }

    /// Whether a socket has something to give a reader, or nothing where it is not a socket.
    ///
    /// End-of-stream counts as readable, as a read shut by `SHUT_RD` does.
    pub(crate) fn readable(&self, node: NodeId, fd: i64) -> Option<bool> {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd))? {
            Socket::Udp { queue, .. } => Some(!queue.is_empty()),
            Socket::Stream { inbox, finished, read_shut, .. } => {
                Some(!inbox.is_empty() || *finished || *read_shut)
            }
            Socket::Listener { pending, .. } => Some(!pending.is_empty()),
        }
    }

    /// Whether a write on a socket would be answered at once, or nothing where it is not a socket.
    ///
    /// A stream takes a write while its send buffer has room and no writer waits before it; one
    /// still connecting takes none. A write that fails at once, on a stream shut or ended, counts
    /// as writable, as Linux reports it. A datagram socket always is; a listener never.
    pub(crate) fn writable(&self, node: NodeId, fd: i64) -> Option<bool> {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd))? {
            Socket::Udp { .. } => Some(true),
            Socket::Stream { peer, remote, finished, write_shut, outbox, writers, .. } => {
                if *write_shut || (peer.is_none() && (*finished || remote.is_none())) {
                    return Some(true);
                }
                Some(peer.is_some() && writers.is_empty() && outbox.len() < STREAM_BUFFER_LIMIT)
            }
            Socket::Listener { .. } => Some(false),
        }
    }

    /// Forgets every receive and write a task was parked in, because the task has ended.
    ///
    /// A delivery would otherwise write into a task the backend no longer has. The socket stays
    /// open, and the bytes a forgotten writer had yet to hand over are never sent.
    pub(crate) fn forget_task(&mut self, task: TaskId) {
        for socket in self.sockets.values_mut() {
            match socket {
                Socket::Udp { waiting, .. } => waiting.retain(|recv| recv.task != task),
                Socket::Stream { waiting, writers, .. } => {
                    waiting.retain(|recv| recv.task != task);
                    writers.retain(|writer| writer.task != task);
                }
                Socket::Listener { waiting, .. } => waiting.retain(|parked| parked.task != task),
            }
        }
    }

    /// Keeps an option a guest set, as it set it; `EBADF` where `fd` is not a socket.
    pub(crate) fn set_option(
        &mut self,
        node: NodeId,
        fd: i64,
        level: u64,
        name: u64,
        value: Vec<u8>,
    ) -> i64 {
        let id = self.resolve(node, fd);
        if !self.sockets.contains_key(&(node, id)) {
            return -errno::EBADF;
        }
        self.options.entry((node, id)).or_default().insert((level, name), value);
        0
    }

    /// What `getsockopt` reads: what the socket is, what was set, or Linux's default.
    ///
    /// The type, domain, protocol and listening state are read off the socket; `SO_ERROR` is
    /// read once. A buffer size reads back doubled, as Linux keeps it. An option nobody set and
    /// the model does not know is `ENOPROTOOPT`.
    pub(crate) fn get_option(
        &mut self,
        node: NodeId,
        fd: i64,
        level: u64,
        name: u64,
    ) -> Result<Vec<u8>, i64> {
        const SOL_SOCKET: u64 = 1;
        const IPPROTO_TCP: u64 = 6;
        let int = |value: i32| value.to_le_bytes().to_vec();
        let id = self.resolve(node, fd);
        let Some(socket) = self.sockets.get(&(node, id)) else {
            return Err(-errno::EBADF);
        };
        let local = self.local_pairs.contains(&(node, id));
        let (kind, protocol, listening) = match socket {
            Socket::Udp { .. } => (2, 17, false),
            Socket::Stream { messages, .. } => {
                (if messages.is_some() { 2 } else { 1 }, if local { 0 } else { 6 }, false)
            }
            Socket::Listener { .. } => (1, 6, true),
        };
        match (level, name) {
            (SOL_SOCKET, 3) => return Ok(int(kind)),
            (SOL_SOCKET, 4) => {
                let held = self.errors.remove(&(node, id)).unwrap_or(0);
                return Ok(int(i32::try_from(held).unwrap_or(0)));
            }
            (SOL_SOCKET, 30) => return Ok(int(i32::from(listening))),
            (SOL_SOCKET, 38) => return Ok(int(protocol)),
            (SOL_SOCKET, 39) => return Ok(int(if local { 1 } else { 2 })),
            _ => {}
        }
        if let Some(value) = self.options.get(&(node, id)).and_then(|set| set.get(&(level, name))) {
            if level == SOL_SOCKET
                && matches!(name, 7 | 8)
                && let Some(size) = value.first_chunk::<4>()
            {
                return Ok(int(i32::from_le_bytes(*size).saturating_mul(2)));
            }
            return Ok(value.clone());
        }
        match (level, name) {
            // `SO_RCVBUF` and `SO_SNDBUF` start where Linux's TCP starts them.
            (SOL_SOCKET, 8) => Ok(int(131_072)),
            (SOL_SOCKET, 7) => Ok(int(16_384)),
            // Flags nobody set: `SO_REUSEADDR`, `SO_DONTROUTE`, `SO_BROADCAST`, `SO_KEEPALIVE`,
            // `SO_OOBINLINE`, `SO_REUSEPORT`.
            (SOL_SOCKET, 2 | 5 | 6 | 9 | 10 | 15) => Ok(int(0)),
            // `SO_LINGER` off.
            (SOL_SOCKET, 13) => Ok(vec![0; 8]),
            // `SO_RCVLOWAT`, `SO_SNDLOWAT`.
            (SOL_SOCKET, 18 | 19) => Ok(int(1)),
            // `SO_RCVTIMEO` and `SO_SNDTIMEO`, old and new spellings: no timeout.
            (SOL_SOCKET, 20 | 21 | 66 | 67) => Ok(vec![0; 16]),
            // `TCP_NODELAY` off.
            (IPPROTO_TCP, 1) => Ok(int(0)),
            _ => Err(-errno::ENOPROTOOPT),
        }
    }

    /// How long a call on `fd` may wait, in ticks: `SO_SNDTIMEO` for a write, `SO_RCVTIMEO` for a
    /// read or an accept; nothing where none was set or it is nought.
    pub(crate) fn timeout(&self, node: NodeId, fd: i64, sending: bool) -> Option<u64> {
        const SOL_SOCKET: u64 = 1;
        let set = self.options.get(&(node, self.resolve(node, fd)))?;
        let names: [u64; 2] = if sending { [21, 67] } else { [20, 66] };
        let value = names.iter().find_map(|name| set.get(&(SOL_SOCKET, *name)))?;
        let seconds = i64::from_le_bytes(*value.first_chunk::<8>()?);
        let micros = i64::from_le_bytes(*value.get(8..16)?.first_chunk::<8>()?);
        if seconds < 0 || micros < 0 {
            return None;
        }
        let ticks = (seconds as u64)
            .saturating_mul(Tick::SECOND.get())
            .saturating_add((micros as u64).saturating_mul(1_000));
        (ticks > 0).then_some(ticks)
    }

    /// Ends the wait a socket timeout was armed for, where the task still waits there.
    ///
    /// The call answers `EAGAIN`, or what it moved before the time ran out. A wait is known by
    /// its task and the moment it parked, so a timer its wait outlived fires into nothing.
    pub(crate) fn time_out(
        &mut self,
        node: NodeId,
        socket: i64,
        task: TaskId,
        since: Tick,
        wakes: &mut Vec<Wake>,
    ) {
        let moved_or_again = |moved: usize| if moved > 0 { moved as i64 } else { -errno::EAGAIN };
        let ret = match self.sockets.get_mut(&(node, socket)) {
            Some(Socket::Udp { waiting, .. }) => {
                let Some(at) = waiting.iter().position(|r| r.task == task && r.since == since)
                else {
                    return;
                };
                waiting.remove(at);
                -errno::EAGAIN
            }
            Some(Socket::Stream { waiting, writers, .. }) => {
                if let Some(at) = waiting.iter().position(|r| r.task == task && r.since == since) {
                    let Some(recv) = waiting.remove(at) else { return };
                    moved_or_again(recv.got)
                } else if let Some(at) =
                    writers.iter().position(|w| w.task == task && w.since == since)
                {
                    let Some(writer) = writers.remove(at) else { return };
                    moved_or_again(writer.written)
                } else {
                    return;
                }
            }
            Some(Socket::Listener { waiting, .. }) => {
                let Some(at) = waiting.iter().position(|p| p.task == task && p.since == since)
                else {
                    return;
                };
                waiting.remove(at);
                -errno::EAGAIN
            }
            None => return,
        };
        wakes.push(Wake { task, ret });
    }

    /// Whether a socket answers `EAGAIN` instead of waiting, or nothing where it is not a socket.
    pub(crate) fn is_nonblocking(&self, node: NodeId, fd: i64) -> Option<bool> {
        match self.sockets.get(&(node, self.resolve(node, fd)))? {
            Socket::Udp { nonblocking, .. }
            | Socket::Stream { nonblocking, .. }
            | Socket::Listener { nonblocking, .. } => Some(*nonblocking),
        }
    }

    /// Sets or clears a socket's non-blocking flag. False where the descriptor is not a socket.
    pub(crate) fn set_nonblocking(&mut self, node: NodeId, fd: i64, on: bool) -> bool {
        let fd = self.resolve(node, fd);
        match self.sockets.get_mut(&(node, fd)) {
            Some(
                Socket::Udp { nonblocking, .. }
                | Socket::Stream { nonblocking, .. }
                | Socket::Listener { nonblocking, .. },
            ) => {
                *nonblocking = on;
                true
            }
            None => false,
        }
    }

    /// Closes a descriptor, and the socket it names once no other descriptor names it.
    ///
    /// The number is free at once. Closing the socket releases its port and ends whatever stream
    /// it carried; a stream closed with bytes in its send buffer lingers until they are
    /// delivered, and its peer reads the end after them.
    pub(crate) fn close(
        &mut self,
        node: NodeId,
        fd: i64,
        numbers: &mut Numbers<'_>,
        ctx: &mut StopCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<i64, KernelError> {
        let Some(id) = self.descriptors.remove(&(node, fd)) else {
            return Ok(-errno::EBADF);
        };
        numbers.release(node, fd);
        let still_named =
            self.descriptors.iter().any(|((at, _), named)| *at == node && *named == id);
        if still_named {
            return Ok(0);
        }
        self.close_socket(node, id, ctx, wakes)
    }

    /// Closes the socket kept under `id`, which no descriptor names any more.
    fn close_socket(
        &mut self,
        node: NodeId,
        fd: i64,
        ctx: &mut StopCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<i64, KernelError> {
        let mut removed = match self.sockets.remove(&(node, fd)) {
            Some(removed) => removed,
            None => return Ok(-errno::EBADF),
        };
        self.options.remove(&(node, fd));
        self.errors.remove(&(node, fd));
        self.edges.remove(&(node, fd));
        self.local_pairs.remove(&(node, fd));
        let Socket::Stream { local, peer, outbox, waiting, writers, write_shut, .. } = &mut removed
        else {
            match removed {
                Socket::Udp { port: Some(port), .. } => {
                    self.release_port(node, Transport::Datagram, port, fd);
                }
                Socket::Listener { port, pending, .. } => {
                    self.release_port(node, Transport::Stream, port, fd);
                    // Connections made and never accepted are dropped, and their callers hear it.
                    for waiting in pending {
                        self.close_socket(node, waiting.fd, ctx, wakes)?;
                    }
                }
                Socket::Udp { .. } | Socket::Stream { .. } => {}
            }
            return Ok(0);
        };
        // Only the holder releases a port; an accepted connection is not one.
        if *local != 0 {
            self.release_port(node, Transport::Stream, *local, fd);
        }
        // Other tasks parked on the descriptor get what they had, and nothing more.
        Self::release_readers(waiting, wakes);
        for writer in writers.drain(..) {
            wakes.push(Wake { task: writer.task, ret: writer.cut_short() });
        }
        *write_shut = true;
        let peer = *peer;
        match peer {
            Some(_) if !outbox.is_empty() => {
                self.lingering.insert((node, fd), removed);
                self.pump((node, fd), ctx, wakes)?;
            }
            Some(peer) => self.finish_peer(peer, wakes),
            None => {}
        }
        Ok(0)
    }

    /// Shuts one or both directions of a stream, as `shutdown(how)` asks.
    ///
    /// `SHUT_RD` ends this side's reads and tells the peer nothing; `SHUT_WR` fails this side's
    /// writes and the peer reads the end once the send buffer is delivered, while the other
    /// direction stays open. A datagram socket or a listener accepts it and nothing changes.
    pub(crate) fn shutdown(
        &mut self,
        node: NodeId,
        fd: i64,
        how: u64,
        ctx: &mut StopCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<i64, KernelError> {
        const SHUT_RD: u64 = 0;
        const SHUT_WR: u64 = 1;
        const SHUT_RDWR: u64 = 2;
        if how > SHUT_RDWR {
            return Ok(-errno::EINVAL);
        }
        let at = (node, self.resolve(node, fd));
        match self.sockets.get_mut(&at) {
            Some(Socket::Stream {
                peer,
                finished,
                read_shut,
                write_shut,
                waiting,
                writers,
                ..
            }) => {
                if peer.is_none() && !*finished {
                    return Ok(-errno::ENOTCONN);
                }
                if how != SHUT_WR {
                    *read_shut = true;
                    Self::release_readers(waiting, wakes);
                    self.edges.entry(at).or_default().arrived += 1;
                }
                if how != SHUT_RD && !*write_shut {
                    *write_shut = true;
                    for writer in writers.drain(..) {
                        wakes.push(Wake { task: writer.task, ret: writer.cut_short() });
                    }
                }
            }
            Some(_) => return Ok(0),
            None => return Ok(-errno::EBADF),
        }
        if how != SHUT_RD {
            self.pump(at, ctx, wakes)?;
        }
        Ok(0)
    }

    /// The bound address of a socket, for `getsockname`.
    pub(crate) fn name_of(&self, node: NodeId, fd: i64) -> Option<[u8; SOCKADDR_IN_BYTES]> {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd)) {
            Some(Socket::Udp { port, .. }) => {
                Some(encode_sockaddr(Endpoint { node, port: port.unwrap_or(0) }))
            }
            Some(Socket::Stream { local, .. }) => {
                Some(encode_sockaddr(Endpoint { node, port: *local }))
            }
            Some(Socket::Listener { port, .. }) => {
                Some(encode_sockaddr(Endpoint { node, port: *port }))
            }
            None => None,
        }
    }

    /// The address at the other end of a stream, for `getpeername`.
    pub(crate) fn peer_name_of(&self, node: NodeId, fd: i64) -> Option<[u8; SOCKADDR_IN_BYTES]> {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd)) {
            Some(Socket::Stream { remote: Some(remote), .. }) => Some(encode_sockaddr(*remote)),
            _ => None,
        }
    }
}

/// Arguments an accept carries, read from the syscall registers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingAcceptArgs {
    /// Where the caller's address goes; zero for "not asked".
    pub(crate) addr: u64,
    /// Where the length of that address goes.
    pub(crate) addr_len: u64,
    /// Whether the descriptor handed out starts non-blocking.
    pub(crate) nonblocking: bool,
}

/// Arguments a receive carries, read from the syscall registers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingRecvArgs {
    /// Where the payload lands.
    pub(crate) buf: GuestAddr,
    /// Capacity of that buffer.
    pub(crate) len: usize,
    /// Where the source address goes; zero for "not asked".
    pub(crate) src_addr: u64,
    /// The receive's [`msg`] flags; zero for a plain `read`.
    pub(crate) flags: u64,
}

/// Reads a `sockaddr_in` out of guest memory, yielding the node and port it names.
pub(crate) fn decode_sockaddr(
    ctx: &mut StopCtx<'_, '_>,
    addr: GuestAddr,
    own: NodeId,
) -> Result<Option<(NodeId, u16)>, KernelError> {
    let mut buf = [0u8; SOCKADDR_IN_BYTES];
    ctx.read_mem(addr, &mut buf)?;
    let family = u16::from_le_bytes([buf[0], buf[1]]);
    if family != AF_INET {
        return Ok(None);
    }
    let port = u16::from_be_bytes([buf[2], buf[3]]);
    let ip = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Ok(node_behind(ip, own).map(|node| (node, port)))
}

/// Builds the `sockaddr_in` for an endpoint.
fn encode_sockaddr(endpoint: Endpoint) -> [u8; SOCKADDR_IN_BYTES] {
    let mut out = [0u8; SOCKADDR_IN_BYTES];
    out[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    out[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    out[4..8].copy_from_slice(&address_of(endpoint.node).to_be_bytes());
    out
}
