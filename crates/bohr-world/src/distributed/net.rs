//! The virtual network: links, partitions, datagrams and byte streams.
//!
//! Semantics are normative in `docs/API.md` §5.3. The load-bearing rule: every packet makes the
//! same four draws — drop, duplicate, reorder, delay — whatever the network's state, and only then
//! is the partition matrix consulted, so a healed partition changes fates and never tape layout.
//!
//! The two kinds of socket are adverse in different ways, and deliberately so. A datagram is drawn
//! upon four times and may be lost, doubled, overtaken or late, because that is what its transport
//! promises and no more. A stream is drawn upon not at all, because its transport retransmits until
//! all four are gone; the one adversity it cannot hide is that the far end is unreachable, and that
//! is the one this model applies to it. A stream conversation therefore costs no entropy, so adding
//! one to a scenario cannot shift a single draw anywhere else in it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bohr_kernel::{KernelError, Wake, WorldCtx};
use bohr_tape::DrawSite;
use bohr_trace::{Endpoint, EventKind, FaultKind, FaultTarget, PacketFate};
use bohr_types::{GuestAddr, NodeId, TaskId, Tick};

use super::draw_odds;
use crate::nums::errno;

/// First descriptor handed out for a socket.
///
/// Sockets are this model's own and files are the per-node world's, and the two allocators never
/// learn about each other; disjoint ranges are what keeps a descriptor unambiguous. Guests treat
/// descriptors as opaque, so the unrealistic values change nothing they can observe.
pub(crate) const SOCKET_FD_BASE: i64 = 4096;

/// Datagrams a receive queue holds; an arrival past this is dropped (API §5.3).
const RECV_QUEUE_LIMIT: usize = 64;

/// Bytes a stream will hold on either side before a write stops being accepted.
///
/// **The supervisor may not be what runs out of memory (§4.4), and these had no ceiling at all.**
/// A guest writing into a stream nobody reads, or one a partition has cut, grew a queue inside the
/// supervisor for as long as the run lasted -- while the datagram queue beside it has been bounded
/// since it was written. A full stream answers the way a full socket buffer does, so a guest sees
/// back-pressure rather than a supervisor that stops.
const STREAM_BUFFER_LIMIT: usize = 1 << 20;

/// First ephemeral port, handed out upward in binding order.
const FIRST_EPHEMERAL_PORT: u16 = 32768;

/// `AF_INET`, the one address family the model speaks.
const AF_INET: u16 = 2;

/// Size of `struct sockaddr_in`.
const SOCKADDR_IN_BYTES: usize = 16;

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
            partitions: Vec::new(),
            grey: Vec::new(),
        }
    }
}

/// A scheduled window in which one directed link degrades without breaking (API §5.3): "not a
/// break, just very slow" is its own bug class, and a partition test never reaches it.
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

/// A scheduled, not drawn, partition (API §5.3): raised at one tick, healed at another.
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
}

/// A stream a cut had stopped, and what it was stopped in the middle of.
#[derive(Clone, Copy, Debug)]
struct Stalled {
    /// Which socket.
    at: (NodeId, i64),
    /// The other end, where the connection was made before the cut fell.
    peer: Option<(NodeId, i64)>,
    /// Where it is connected, or was trying to be.
    remote: Option<Endpoint>,
    /// How many bytes are waiting to go.
    held: usize,
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
        /// A datagram socket addresses every send explicitly, so this is empty until the guest
        /// says otherwise -- and connect-then-send is how a stub resolver talks, so forgetting it
        /// sent the call looking for a stream and ended the run blaming a descriptor.
        peer: Option<Endpoint>,
        queue: VecDeque<Arrived>,
        /// Tasks parked in `recvfrom`, oldest first.
        ///
        /// A queue and not one slot: several threads reading one datagram socket is the ordinary
        /// shape of a server, and a single slot would silently drop the first reader's wait and
        /// leave it parked with nothing left to release it. The kernel wakes an unspecified one of
        /// them; the longest-waiting is a legal choice and the only one a replay can reconstruct.
        waiting: VecDeque<PendingRecv>,
        /// Set by `FIONBIO`: an empty queue answers `EAGAIN` instead of parking the task, which
        /// is what an event loop asks for when it drains a socket between ticks.
        nonblocking: bool,
    },
    /// One end of a byte stream: a `socketpair`, or one side of a connection.
    ///
    /// **A stream makes no draws.** The transport it stands for hides loss, duplication and
    /// reordering from the program above it by retransmitting until they are gone, so a model that
    /// injected any of the three would hand a guest a conversation the real thing cannot produce —
    /// and every finding that came of it would be about this model. What the transport does not
    /// hide is a partition, and that is the one thing here that acts on a stream.
    Stream {
        /// The port this end answers on; zero where it was never bound.
        local: u16,
        /// The other end, absent while a connection is being made and once it is gone.
        peer: Option<(NodeId, i64)>,
        /// Where this end is connected or is trying to be. A `socketpair` has none: its two ends
        /// have no addresses at all.
        remote: Option<Endpoint>,
        /// Bytes that arrived and have not been read.
        inbox: VecDeque<u8>,
        /// Bytes written while the link was cut, waiting for it to come back.
        outbox: VecDeque<u8>,
        /// The other end will send nothing further, so a read finds the end of the stream instead
        /// of waiting for bytes that are not coming. This is how a guest learns its peer died.
        finished: bool,
        waiting: VecDeque<PendingRecv>,
        nonblocking: bool,
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

/// The cluster's links, sockets and packets on the wire.
#[derive(Debug, Default)]
pub(crate) struct Network {
    config: NetworkConfig,
    sockets: BTreeMap<(NodeId, i64), Socket>,
    /// Descriptors that are copies, each naming the socket it was copied from.
    ///
    /// A runtime duplicates a socket so two parts of it hold handles of their own, and two
    /// descriptors naming one socket is what a copy means. Kept as a name for the original rather
    /// than a second socket: a copy with a queue of its own would be a different socket wearing
    /// the same number, and a datagram delivered to one of them would be invisible to the
    /// other.
    aliases: BTreeMap<(NodeId, i64), i64>,
    /// Which descriptor each bound port belongs to.
    /// Which descriptors hold each port, in the order they took it.
    ///
    /// Several, because `SO_REUSEPORT` is how a server spreads one port across its threads and how
    /// a validator opens its broadcast sockets. One descriptor per port refused the second bind
    /// with `EADDRINUSE`, which no real machine would have reported to a program that asked for
    /// sharing.
    ports: BTreeMap<(NodeId, Transport, u16), Vec<i64>>,
    /// Descriptors whose owner asked that the port be shareable, before it was bound.
    ///
    /// Kept beside the socket rather than inside it because the question is asked of a number and
    /// not of a kind: both sorts of socket carry the option and both are asked the same thing.
    shareable: BTreeSet<(NodeId, i64)>,
    next_fd: BTreeMap<NodeId, i64>,
    next_ephemeral: BTreeMap<NodeId, u16>,
    /// Directed pairs currently cut.
    cuts: BTreeSet<(NodeId, NodeId)>,
    /// Open grey windows per directed link, by index into the configuration.
    grey_open: BTreeMap<(NodeId, NodeId), usize>,
    /// Packets on the wire, by the identifier their delivery timer carries.
    in_flight: BTreeMap<u64, InFlight>,
    next_packet: u64,
    /// Delivery ticks of packets on the wire per directed link, for the reorder rule.
    on_link: BTreeMap<(NodeId, NodeId), Vec<(u64, Tick)>>,
}

/// The virtual address node `n` owns: `10.66.0.(n+1)`.
fn address_of(node: NodeId) -> u32 {
    0x0A42_0000 | (node.get() + 1)
}

/// The node behind a virtual address; loopback and the wildcard both mean "this node".
///
/// A server binds the wildcard because it does not want to know which of its addresses callers
/// will use. A node here has exactly one, so the question does not arise and the answer is the node
/// itself — and refusing it instead, which is what an unrecognised address gets, turns "listen
/// anywhere" into `EINVAL` and stops every server that did not name an address it could not have
/// known.
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

    fn linked(&self, a: NodeId, b: NodeId) -> bool {
        let n = self.config.nodes.max(1);
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
        ctx: &mut WorldCtx<'_, '_>,
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
    /// Asked after a heal, because one entry lifting is not the cluster healing: a scenario with
    /// two overlapping partitions leaves the second in place, and a window opened there would be
    /// measuring a silence the cluster has every reason to keep.
    pub(crate) fn fully_healed(&self) -> bool {
        self.cuts.is_empty()
    }

    /// Adds a partition the run drew rather than one a scenario wrote, and answers its index.
    ///
    /// The same list the scheduled ones live in, so a drawn cut and a written one are raised,
    /// healed and narrated by one piece of code.
    pub(crate) fn add_partition(&mut self, partition: ScheduledPartition) -> usize {
        self.config.partitions.push(partition);
        self.config.partitions.len() - 1
    }

    /// Adds a degradation the run drew, and answers its index. As [`Self::add_partition`], and for
    /// the same reason.
    pub(crate) fn add_grey(&mut self, window: GreyWindow) -> usize {
        self.config.grey.push(window);
        self.config.grey.len() - 1
    }

    /// Every directed link between `node` and the rest of the cluster.
    ///
    /// A whole node cut off rather than one link of a pair chosen by a draw: isolation is what a
    /// campaign wants far more often, and it needs nothing drawn beyond the node the fault was
    /// already aimed at — so arming it costs a fire and not a byte.
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
        ctx: &mut WorldCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let Some(partition) = self.config.partitions.get(index).cloned() else {
            return Err(KernelError::Invariant("a partition timer names a missing entry"));
        };
        for (from, to) in partition.cut {
            if raise {
                self.cuts.insert((from, to));
            } else {
                self.cuts.remove(&(from, to));
            }
            // The cut is a world-level fact with no task behind it; the `from` node carries the
            // event so a per-node timeline shows its own connectivity changing.
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
            self.resume_streams(ctx, wakes)?;
        }
        Ok(())
    }

    /// Carries on every stream the partition had stopped.
    ///
    /// Two things were held rather than lost: bytes written into a cut link, and a connection whose
    /// handshake never arrived. Both are what the transport would have kept retrying, so both
    /// happen now, in the order the sockets are held rather than the order they stalled — a stream
    /// promises its own bytes stay in order, and says nothing about two of them against each other.
    fn resume_streams(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let stalled: Vec<Stalled> = self
            .sockets
            .iter()
            .filter_map(|(at, socket)| match socket {
                Socket::Stream { peer, remote, outbox, .. } if !outbox.is_empty() => {
                    Some(Stalled { at: *at, peer: *peer, remote: *remote, held: outbox.len() })
                }
                Socket::Stream { peer: None, remote: Some(to), finished: false, .. } => {
                    Some(Stalled { at: *at, peer: None, remote: Some(*to), held: 0 })
                }
                _ => None,
            })
            .collect();

        for stream in stalled {
            match (stream.peer, stream.remote) {
                (Some(peer), _) if stream.held > 0 && self.reaches(stream.at.0, peer.0) => {
                    let Some(Socket::Stream { outbox, .. }) = self.sockets.get_mut(&stream.at)
                    else {
                        continue;
                    };
                    let bytes: Vec<u8> = outbox.drain(..).collect();
                    Self::land(&mut self.sockets, peer, &bytes, ctx, wakes)?;
                }
                (None, Some(to)) if self.reaches(stream.at.0, to.node) => {
                    let port = self.local_port(stream.at.0, stream.at.1);
                    let from = Endpoint { node: stream.at.0, port };
                    if self.complete_connect(stream.at.0, stream.at.1, from, to, ctx, wakes)? != 0 {
                        // Nobody was listening after all. The caller has already been told the
                        // attempt was under way, so the refusal reaches it the only way a started
                        // connection can end: as a stream with nothing left in it.
                        self.finish_peer(stream.at, wakes);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Opens a datagram socket and returns its descriptor.
    pub(crate) fn open_udp(&mut self, node: NodeId, owner: TaskId, nonblocking: bool) -> i64 {
        let fd = self.allocate_fd(node);
        self.sockets.insert(
            (node, fd),
            Socket::Udp {
                owner,
                port: None,
                peer: None,
                queue: VecDeque::new(),
                waiting: VecDeque::new(),
                nonblocking,
            },
        );
        fd
    }

    /// Whether a descriptor is a stream, which decides what a connect on it means.
    pub(crate) fn is_stream(&self, node: NodeId, fd: i64) -> bool {
        let fd = self.resolve(node, fd);
        matches!(self.sockets.get(&(node, fd)), Some(Socket::Stream { .. }))
    }

    /// Opens an unconnected stream socket.
    pub(crate) fn open_stream(&mut self, node: NodeId, nonblocking: bool) -> i64 {
        let fd = self.allocate_fd(node);
        self.sockets.insert((node, fd), Self::stream(0, None, None, nonblocking));
        fd
    }

    /// Opens a connected pair of intra-node stream sockets.
    pub(crate) fn open_pair(&mut self, node: NodeId) -> (i64, i64) {
        let a = self.allocate_fd(node);
        let b = self.allocate_fd(node);
        self.sockets.insert((node, a), Self::stream(0, Some((node, b)), None, false));
        self.sockets.insert((node, b), Self::stream(0, Some((node, a)), None, false));
        (a, b)
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
            waiting: VecDeque::new(),
            nonblocking,
        }
    }

    /// The socket a descriptor really names, following a copy.
    ///
    /// Applied at every call a guest makes, and never inside: once a call has resolved its own
    /// descriptor, the numbers it works with afterwards are the model's own and already true.
    pub(crate) fn resolve(&self, node: NodeId, fd: i64) -> i64 {
        self.aliases.get(&(node, fd)).copied().unwrap_or(fd)
    }

    /// Copies a descriptor, answering with the new one, or nothing where it is not a socket.
    pub(crate) fn dup(&mut self, node: NodeId, fd: i64) -> Option<i64> {
        let original = self.resolve(node, fd);
        if !self.sockets.contains_key(&(node, original)) {
            return None;
        }
        let copy = self.allocate_fd(node);
        self.aliases.insert((node, copy), original);
        Some(copy)
    }

    fn allocate_fd(&mut self, node: NodeId) -> i64 {
        let next = self.next_fd.entry(node).or_insert(SOCKET_FD_BASE);
        let fd = *next;
        *next += 1;
        fd
    }

    pub(crate) fn is_socket(&self, node: NodeId, fd: i64) -> bool {
        let fd = self.resolve(node, fd);
        self.sockets.contains_key(&(node, fd))
    }

    /// Binds a socket to a port, `0` meaning "hand me an ephemeral one".
    /// Whether this node holds any socket at all.
    ///
    /// Asked by the call router: a call this world does not route to the network may still be
    /// answered for a node that has none, and must not be for a node that has one.
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

    /// Which of the two port spaces a socket lives in.
    ///
    /// Linux gives datagrams and streams a space each, so one number is two endpoints and a
    /// service binding both is doing the ordinary thing. Keyed together, the second bind of the
    /// pair was answered `EADDRINUSE` -- and a validator that binds its gossip port for both
    /// stopped there, on a conflict no real machine would have reported.
    /// Gives up one descriptor's claim on a port, leaving whatever else holds it.
    ///
    /// A claim is released only by the descriptor that made it. An accepted connection carries the
    /// listening port so `getsockname` can answer and is not what that port is registered to, so
    /// letting any holder clear the entry left a server bound to nothing the moment it finished
    /// with its first caller.
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
    /// **One holder is the ordinary case and costs no draw.** Where several share a port the
    /// kernel picks between them by a rule of its own -- unspecified, and not the same twice -- so
    /// the choice is made from the tape instead. A replay makes the same choice; a campaign that
    /// wants the other one asks for another seed.
    fn holder(
        &mut self,
        ctx: &mut WorldCtx<'_, '_>,
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
        // The port is taken, which Linux says with `EADDRINUSE`. `EINVAL` sent a server looking
        // for a malformed request it had not made.
        // A port already held is taken only where everybody involved asked for sharing: the
        // descriptors holding it and the one asking. Linux wants the option on all of them, and a
        // model that wanted it on none would let a program bind over a port it had no business
        // taking -- a conflict it would then never find out about.
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
    /// The backlog is read and not kept. What a full backlog does is drop the handshake, leaving
    /// the caller to retransmit until there is room; this model has no retransmission to stall, so
    /// honouring the number would have to invent a refusal the real thing does not give.
    pub(crate) fn listen(&mut self, node: NodeId, fd: i64) -> i64 {
        let fd = self.resolve(node, fd);
        let Some(Socket::Stream { local, peer: None, nonblocking, .. }) =
            self.sockets.get(&(node, fd))
        else {
            return -errno::EINVAL;
        };
        let port = *local;
        // **Carried over, not started afresh.** Listening changes what a descriptor is for and
        // nothing about how it behaves: a socket opened not to block does not begin blocking
        // because it was told to listen. Losing the flag here parks an event loop in its own
        // `accept` — for ever, because the loop that would have given it something to accept is
        // the one now parked.
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
    /// A cut link swallows the handshake rather than refusing it: the caller is left connecting,
    /// which is what a real one does until it gives up, and the attempt is finished if the link
    /// comes back. Refusing outright would report in an instant a verdict the transport takes
    /// minutes to reach, and would hide the very stall a partition exists to cause.
    pub(crate) fn connect(
        &mut self,
        node: NodeId,
        fd: i64,
        to: Endpoint,
        ctx: &mut WorldCtx<'_, '_>,
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
        self.complete_connect(node, fd, Endpoint { node, port: source }, to, ctx, wakes)
    }

    /// Joins a caller to the listener it named, or refuses where nobody is listening.
    fn complete_connect(
        &mut self,
        node: NodeId,
        fd: i64,
        from: Endpoint,
        to: Endpoint,
        ctx: &mut WorldCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<i64, KernelError> {
        let listener = self.holder(ctx, to.node, Transport::Stream, to.port)?;
        match listener.and_then(|l| self.sockets.get(&(to.node, l))) {
            Some(Socket::Listener { .. }) => {}
            _ => return Ok(-errno::ECONNREFUSED),
        }
        let listener = listener.unwrap_or_default();

        // The listening end is made now rather than at the accept, because a caller may write as
        // soon as its own end is connected and those bytes have to land somewhere.
        let accepted = self.allocate_fd(to.node);
        self.sockets.insert(
            (to.node, accepted),
            Self::stream(to.port, Some((node, fd)), Some(from), false),
        );
        if let Some(Socket::Stream { peer, .. }) = self.sockets.get_mut(&(node, fd)) {
            *peer = Some((to.node, accepted));
        }

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
                wakes.push(Wake { task: parked.task, ret: accepted });
            }
            None => pending.push_back(Pending { fd: accepted, peer: from }),
        }
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
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Option<i64>, KernelError> {
        let fd = self.resolve(node, fd);
        let task = ctx.task();
        let Some(Socket::Listener { pending, waiting, nonblocking, .. }) =
            self.sockets.get_mut(&(node, fd))
        else {
            return Ok(Some(-errno::EINVAL));
        };
        let parked = PendingAccept {
            task,
            addr: want.addr,
            addr_len: want.addr_len,
            nonblocking: want.nonblocking,
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
        Ok(Some(ready.fd))
    }

    /// Writes the caller's address where an accept asked for it.
    fn hand_over(
        ctx: &mut WorldCtx<'_, '_>,
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
        from == to || (self.linked(from, to) && !self.cuts.contains(&(from, to)))
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
        ctx: &mut WorldCtx<'_, '_>,
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

        // The four draws, always, in the documented order (API §5.3): state is consulted only
        // after the tape has spoken, so state can never move the tape. Probabilities go through
        // a fixed-width comparison rather than `draw_bool`, whose certainty shortcut would let a
        // configured — or grey-overridden — zero change how many bytes a packet costs.
        let dropped = draw_odds(ctx, drop_probability, DrawSite::NetDeliver)?;
        let duplicated = draw_odds(ctx, self.config.duplicate_probability, DrawSite::NetDeliver)?;
        let reordered = draw_odds(ctx, self.config.reorder_probability, DrawSite::NetReorder)?;
        let jitter = ctx.draw_exponential(self.config.latency_jitter_ticks, DrawSite::NetDelay)?;
        let delay = mean.saturating_add(jitter);

        let unreachable = !self.linked(node, dest_node);
        let cut = self.cuts.contains(&(node, dest_node));
        if dropped || unreachable || cut {
            ctx.emit(EventKind::PacketFate { fate: PacketFate::Dropped, delay_ticks: delay })?;
            return Ok(len);
        }

        let now = ctx.world_now();
        let natural = Tick(now.get().saturating_add(delay).max(now.get() + 1));
        // Reordering slots this packet just before the earliest one still flying on the same
        // directed link; with an empty link the fate degrades to an ordinary delivery.
        let at = if reordered {
            match self
                .on_link
                .get(&(node, dest_node))
                .and_then(|flights| flights.iter().map(|(_, at)| *at).min())
            {
                Some(earliest) => Tick(earliest.get().saturating_sub(1).max(now.get() + 1)),
                None => natural,
            }
        } else {
            natural
        };

        // Copied only where a duplicate was drawn. Every datagram would otherwise pay for the one
        // in a thousand that is sent twice.
        let copy = duplicated.then(|| payload.clone());
        self.enqueue(
            node,
            dest_node,
            InFlight { from, to, payload, delay, duplicated: false },
            at,
            arm,
        );
        if let Some(payload) = copy {
            self.enqueue(
                node,
                dest_node,
                InFlight { from, to, payload, delay, duplicated: true },
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
        ctx: &mut WorldCtx<'_, '_>,
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
        let fate = if packet.duplicated { PacketFate::Duplicated } else { PacketFate::Delivered };
        let Some(fd) = self.holder(ctx, dest, Transport::Datagram, packet.to.port)? else {
            // Nobody listens on the port: the packet evaporates, which is what UDP promises.
            ctx.emit_for(
                dest,
                TaskId::ZERO,
                EventKind::PacketFate { fate: PacketFate::Dropped, delay_ticks: packet.delay },
            )?;
            return Ok(());
        };
        let Some(Socket::Udp { owner, queue, waiting, .. }) = self.sockets.get_mut(&(dest, fd))
        else {
            return Err(KernelError::Invariant("a bound port names a missing socket"));
        };
        let owner = *owner;

        if let Some(recv) = waiting.pop_front() {
            let taken = packet.payload.len().min(recv.len);
            ctx.write_mem_for(recv.task, recv.buf, &packet.payload[..taken])?;
            if recv.src_addr != 0 {
                let addr = encode_sockaddr(packet.from);
                ctx.write_mem_for(recv.task, GuestAddr(recv.src_addr), &addr)?;
            }
            ctx.emit_for(
                dest,
                recv.task,
                EventKind::PacketFate { fate, delay_ticks: packet.delay },
            )?;
            wakes.push(Wake { task: recv.task, ret: taken as i64 });
            return Ok(());
        }

        if queue.len() >= RECV_QUEUE_LIMIT {
            ctx.emit_for(
                dest,
                owner,
                EventKind::PacketFate { fate: PacketFate::Dropped, delay_ticks: packet.delay },
            )?;
            return Ok(());
        }
        queue.push_back(Arrived { from: packet.from, payload: packet.payload });
        ctx.emit_for(dest, owner, EventKind::PacketFate { fate, delay_ticks: packet.delay })?;
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
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<Option<i64>, KernelError> {
        let fd = self.resolve(node, fd);
        let task = ctx.task();
        match self.sockets.get_mut(&(node, fd)) {
            Some(Socket::Udp { queue, waiting, nonblocking, .. }) => {
                if queue.is_empty() && *nonblocking {
                    return Ok(Some(-errno::EAGAIN));
                }
                if let Some(arrived) = queue.pop_front() {
                    let taken = arrived.payload.len().min(recv.len);
                    ctx.write_mem(recv.buf, &arrived.payload[..taken])?;
                    if recv.src_addr != 0 {
                        let addr = encode_sockaddr(arrived.from);
                        ctx.write_mem(GuestAddr(recv.src_addr), &addr)?;
                    }
                    return Ok(Some(taken as i64));
                }
                waiting.push_back(PendingRecv {
                    task,
                    buf: recv.buf,
                    len: recv.len,
                    src_addr: recv.src_addr,
                });
                Ok(None)
            }
            // A stream end reads its own inbox, which is where its peer's writes land. An empty
            // inbox on a finished stream is the end of it — nought, and never a wait, because
            // nothing is coming and a task parked here would never be released.
            Some(Socket::Stream { inbox, waiting, finished, nonblocking, .. }) => {
                if inbox.is_empty() {
                    if *finished {
                        return Ok(Some(0));
                    }
                    if *nonblocking {
                        return Ok(Some(-errno::EAGAIN));
                    }
                    waiting.push_back(PendingRecv {
                        task,
                        buf: recv.buf,
                        len: recv.len,
                        src_addr: 0,
                    });
                    return Ok(None);
                }
                let taken = inbox.len().min(recv.len);
                let bytes: Vec<u8> = inbox.drain(..taken).collect();
                ctx.write_mem(recv.buf, &bytes)?;
                Ok(Some(taken as i64))
            }
            Some(Socket::Listener { .. }) => Ok(Some(-errno::ENOTCONN)),
            None => Ok(Some(-errno::EBADF)),
        }
    }

    /// Writes into a stream, waking a parked reader on the other end.
    ///
    /// A cut link holds the bytes on this side instead of losing them: the transport this stands
    /// for accepts a write into its own buffer and keeps trying, so the caller learns nothing from
    /// the write itself. What it learns is that no answer comes back.
    pub(crate) fn stream_write(
        &mut self,
        node: NodeId,
        fd: i64,
        payload: &[u8],
        ctx: &mut WorldCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<i64, KernelError> {
        let fd = self.resolve(node, fd);
        let Some(Socket::Stream { peer, finished, .. }) = self.sockets.get(&(node, fd)) else {
            return Ok(-errno::EBADF);
        };
        let Some(peer) = *peer else {
            // Gone means the other end closed; still connecting means it never arrived.
            return Ok(if *finished { -errno::EPIPE } else { -errno::ENOTCONN });
        };
        if !self.reaches(node, peer.0) {
            if let Some(Socket::Stream { outbox, nonblocking, .. }) =
                self.sockets.get_mut(&(node, fd))
            {
                // What will not fit does not go. A blocking socket is told to try again rather
                // than parked, because nothing here empties an outbox until the cut heals: the
                // answer a real kernel gives a full buffer is the honest one either way.
                let room = STREAM_BUFFER_LIMIT.saturating_sub(outbox.len());
                if room == 0 {
                    let _ = nonblocking;
                    return Ok(-errno::EAGAIN);
                }
                let take = room.min(payload.len());
                outbox.extend(payload[..take].iter().copied());
                return Ok(take as i64);
            }
            return Ok(payload.len() as i64);
        }
        Self::land(&mut self.sockets, peer, payload, ctx, wakes)?;
        Ok(payload.len() as i64)
    }

    /// Puts bytes into a stream's inbox and releases whoever was waiting for them.
    fn land(
        sockets: &mut BTreeMap<(NodeId, i64), Socket>,
        at: (NodeId, i64),
        payload: &[u8],
        ctx: &mut WorldCtx<'_, '_>,
        wakes: &mut Vec<Wake>,
    ) -> Result<(), KernelError> {
        let Some(Socket::Stream { inbox, waiting, .. }) = sockets.get_mut(&at) else {
            return Ok(());
        };
        // Bounded for the same reason the outbox is: a peer that never reads is a peer whose bytes
        // would otherwise accumulate in the supervisor. What does not fit is dropped, which is
        // what a receive buffer at its limit does.
        let room = STREAM_BUFFER_LIMIT.saturating_sub(inbox.len());
        inbox.extend(payload[..room.min(payload.len())].iter().copied());
        while let Some(recv) = waiting.pop_front() {
            if inbox.is_empty() {
                waiting.push_front(recv);
                break;
            }
            let taken = inbox.len().min(recv.len);
            let bytes: Vec<u8> = inbox.drain(..taken).collect();
            ctx.write_mem_for(recv.task, recv.buf, &bytes)?;
            wakes.push(Wake { task: recv.task, ret: taken as i64 });
        }
        Ok(())
    }

    /// Tells a stream's peer that nothing further is coming, releasing any reader parked on it.
    ///
    /// The one way a guest learns its counterpart is gone, and so the one way a client whose
    /// server was killed mid-request finds out rather than waiting forever.
    fn finish_peer(&mut self, peer: (NodeId, i64), wakes: &mut Vec<Wake>) {
        let Some(Socket::Stream { finished, peer: theirs, waiting, .. }) =
            self.sockets.get_mut(&peer)
        else {
            return;
        };
        *finished = true;
        *theirs = None;
        // Nought is the end of the stream, and it writes nothing into the reader's buffer, so a
        // release here needs no memory of the task it releases.
        for recv in waiting.drain(..) {
            wakes.push(Wake { task: recv.task, ret: 0 });
        }
    }

    /// Forgets everything a node's process held: its sockets and its ports. Packets already
    /// flying toward it will find nobody listening and evaporate, which is what UDP promises.
    ///
    /// Every stream it was carrying ends, and the end is told to whoever was on the other side.
    /// A node dies without closing anything, and a peer left waiting on a connection that no
    /// longer has a process behind it would wait for the rest of the run.
    pub(crate) fn drop_node(&mut self, node: NodeId, wakes: &mut Vec<Wake>) {
        let bereaved: Vec<(NodeId, i64)> = self
            .sockets
            .iter()
            .filter(|((owner, _), _)| *owner == node)
            .filter_map(|(_, socket)| match socket {
                Socket::Stream { peer: Some(peer), .. } if peer.0 != node => Some(*peer),
                _ => None,
            })
            .collect();
        self.sockets.retain(|(owner, _), _| *owner != node);
        self.ports.retain(|(owner, _, _), _| *owner != node);
        self.shareable.retain(|(owner, _)| *owner != node);
        self.aliases.retain(|(owner, _), _| *owner != node);
        self.next_fd.remove(&node);
        self.next_ephemeral.remove(&node);
        for peer in bereaved {
            self.finish_peer(peer, wakes);
        }
    }

    /// Whether a socket has something to give a reader, or nothing where it is not a socket.
    ///
    /// The one question a readiness set asks of the network. Writability is not asked because this
    /// model has no send buffer to fill: a datagram is drawn upon and either flies or does not, and
    /// a stream takes whatever it is given, so a socket is always ready to be written to.
    ///
    /// The end of a stream counts as something to give. A poller told a finished stream is quiet
    /// never wakes its reader, and the reader is exactly the one who needs to hear it.
    pub(crate) fn readable(&self, node: NodeId, fd: i64) -> Option<bool> {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd))? {
            Socket::Udp { queue, .. } => Some(!queue.is_empty()),
            Socket::Stream { inbox, finished, .. } => Some(!inbox.is_empty() || *finished),
            Socket::Listener { pending, .. } => Some(!pending.is_empty()),
        }
    }

    /// Forgets every receive a task was parked in, because the task has ended.
    ///
    /// A delivery writes into the receiver's memory, so a parked task that ended and was left in a
    /// queue is a write to a task the backend no longer has — `BOHR-3005`, raised at the delivery
    /// and blamed on whatever happened to be running. The socket outlives the thread that was
    /// reading it: several threads share one, and one of them ending closes nothing.
    pub(crate) fn forget_task(&mut self, task: TaskId) {
        for socket in self.sockets.values_mut() {
            match socket {
                Socket::Udp { waiting, .. } | Socket::Stream { waiting, .. } => {
                    waiting.retain(|recv| recv.task != task);
                }
                Socket::Listener { waiting, .. } => waiting.retain(|parked| parked.task != task),
            }
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

    /// Closes a socket, releasing its port and ending whatever stream it carried.
    pub(crate) fn close(&mut self, node: NodeId, fd: i64, wakes: &mut Vec<Wake>) -> i64 {
        // A copy going away takes the copy and nothing else. The socket is still named by whoever
        // it was copied from, and ending a conversation is not what a runtime does when it drops a
        // spare handle on it.
        if self.aliases.remove(&(node, fd)).is_some() {
            return 0;
        }
        let removed = match self.sockets.remove(&(node, fd)) {
            Some(removed) => removed,
            None => return -errno::EBADF,
        };
        match removed {
            Socket::Udp { port: Some(port), .. } => {
                self.release_port(node, Transport::Datagram, port, fd);
            }
            Socket::Listener { port, pending, .. } => {
                self.release_port(node, Transport::Stream, port, fd);
                // Connections made and never accepted are dropped, and their callers hear it.
                for waiting in pending {
                    self.close(node, waiting.fd, wakes);
                }
            }
            Socket::Stream { local, peer, .. } => {
                // A port is given up by whoever holds it, and an accepted connection does not: it
                // carries the listening port so `getsockname` can answer, and is not what that
                // port is registered to. Letting it unregister left a server bound to nothing the
                // moment it finished with its first caller — one request served and every one
                // after it refused.
                if local != 0 {
                    self.release_port(node, Transport::Stream, local, fd);
                }
                if let Some(peer) = peer {
                    self.finish_peer(peer, wakes);
                }
            }
            Socket::Udp { .. } => {}
        }
        0
    }

    /// Ends the sending half of a stream, which the other end reads as the end of it.
    pub(crate) fn shutdown(&mut self, node: NodeId, fd: i64, wakes: &mut Vec<Wake>) -> i64 {
        let fd = self.resolve(node, fd);
        match self.sockets.get(&(node, fd)) {
            Some(Socket::Stream { peer: Some(peer), .. }) => {
                let peer = *peer;
                self.finish_peer(peer, wakes);
                0
            }
            // A datagram socket has no half to shut: the call is accepted and changes nothing,
            // which is what it did before streams existed here.
            Some(_) => 0,
            None => -errno::EBADF,
        }
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
}

/// Reads a `sockaddr_in` out of guest memory, yielding the node and port it names.
pub(crate) fn decode_sockaddr(
    ctx: &mut WorldCtx<'_, '_>,
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
