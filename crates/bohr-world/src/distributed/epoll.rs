//! Readiness: which descriptors have something to offer, and who is waiting to hear it.
//!
//! Every asynchronous runtime in Linux is built on this call, and it is what stands between this
//! engine and every network client written on one. What it needs from the world is not new
//! behaviour but a new *question*: the model already knows whether a socket has a datagram waiting,
//! and `epoll` is the way a guest asks about many descriptors at once instead of blocking on one.
//!
//! **It lives in the cluster model rather than in a node's own world, because it is the only place
//! that sees the whole descriptor space.** A node's files belong to its own world and its sockets
//! belong to the network; a readiness set spans both, and a model that saw one half would answer
//! confidently about the other.

use std::collections::{BTreeMap, VecDeque};

use bohr_types::{GuestAddr, NodeId, TaskId};

/// First descriptor handed out for an epoll instance.
///
/// A range of its own, clear of files (which start at three) and sockets (at 4096): the three
/// allocators never learn about each other, and disjoint ranges are what keeps a descriptor
/// unambiguous. Guests treat descriptors as opaque, so the unrealistic values change nothing.
pub(crate) const EPOLL_FD_BASE: i64 = 8192;

/// First descriptor handed out for an event counter.
pub(crate) const EVENTFD_BASE: i64 = 12288;

/// `EPOLL_CTL_ADD`, `_DEL`, `_MOD`.
pub(crate) const CTL_ADD: u64 = 1;
/// Remove a descriptor from the set.
pub(crate) const CTL_DEL: u64 = 2;
/// Change what is watched for.
pub(crate) const CTL_MOD: u64 = 3;

/// The descriptor has something to read.
pub(crate) const EPOLLIN: u32 = 0x001;
/// The descriptor will accept a write.
pub(crate) const EPOLLOUT: u32 = 0x004;
/// Report a descriptor once, when it becomes ready, rather than for as long as it is.
pub(crate) const EPOLLET: u32 = 1 << 31;
/// Report a descriptor once, and then not again until it is modified.
pub(crate) const EPOLLONESHOT: u32 = 1 << 30;

/// Size of `struct epoll_event` on x86-64, which is packed: a `u32` and a `u64` with no padding.
pub(crate) const EVENT_BYTES: usize = 12;

/// What one watched descriptor asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Watch {
    /// The events the guest wants to hear about.
    events: u32,
    /// The guest's own token, handed back untouched. Never interpreted: it is a pointer or an
    /// index in the guest's world, and a model that read it would be reading somebody else's
    /// bookkeeping.
    token: u64,
    /// The readiness epoch this descriptor was last reported in, for an edge-triggered watch.
    seen: Option<u64>,
    /// Whether a one-shot watch has fired, and so reports nothing until it is modified.
    fired: bool,
}

/// A task parked in `epoll_wait` or `poll`, and where its answer goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Parked {
    task: TaskId,
    /// Where the ready list is written, or the `pollfd` array for a `poll`.
    out: GuestAddr,
    /// How many entries the buffer holds.
    max: usize,
    /// Whether the answer is `pollfd` entries rather than `epoll_event`s.
    poll: bool,
}

/// A parked task as it is taken back: who, where the answer goes, how much fits, and its shape.
pub(crate) type Taken = (TaskId, GuestAddr, usize, bool);

/// One epoll instance.
#[derive(Debug, Default)]
struct Instance {
    watched: BTreeMap<i64, Watch>,
    /// Tasks waiting for something to become ready, oldest first.
    ///
    /// A queue for the same reason a socket's is: several threads may wait on one instance, and a
    /// single slot would drop the first one's wait without a word.
    parked: VecDeque<Parked>,
}

/// What a node was asked to report readiness on.
#[derive(Debug, Default)]
pub(crate) struct Epolls {
    instances: BTreeMap<(NodeId, i64), Instance>,
    /// Event counters, by the descriptor each was opened as.
    ///
    /// Kept beside the epoll sets because they exist for the same reason: a runtime uses one to
    /// wake its own poller, and a poller that could not be woken is a runtime that never returns.
    counters: BTreeMap<(NodeId, i64), u64>,
    /// Duplicate descriptors, each naming the instance it was copied from.
    ///
    /// A runtime duplicates its poller so that the thread which wakes it holds a handle of its
    /// own, and two descriptors naming one instance is what `dup` means. Kept as an alias rather
    /// than a second instance, because a copy that watched its own set would be a different poller
    /// wearing the same number.
    aliases: BTreeMap<(NodeId, i64), i64>,
    next_epoll: BTreeMap<NodeId, i64>,
    next_counter: BTreeMap<NodeId, i64>,
    /// The next number for an instance a `poll` makes for itself.
    next_hidden: BTreeMap<NodeId, i64>,
    /// Counts the moments something may have become ready: a delivery, a write to a counter, a
    /// heal. An edge-triggered watch reports a descriptor once per epoch, which is what
    /// "when it becomes ready" means to a model whose readiness only changes at those moments.
    epoch: u64,
}

/// Where the instances a `poll` makes for itself are numbered from: past every number a guest can
/// hold, so closing one never reaches a descriptor of the guest's, however many polls a run makes.
pub(crate) const HIDDEN_BASE: i64 = 1 << 40;

/// One descriptor that became ready, as `epoll_wait` reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Ready {
    /// The descriptor, which the guest is never shown: it reads the token.
    pub(crate) fd: i64,
    /// What happened.
    pub(crate) events: u32,
    /// The guest's token for the descriptor it happened on.
    pub(crate) token: u64,
}

impl Ready {
    /// The twelve bytes `struct epoll_event` is, packed as x86-64 packs it.
    pub(crate) fn encode(self) -> [u8; EVENT_BYTES] {
        let mut out = [0u8; EVENT_BYTES];
        out[0..4].copy_from_slice(&self.events.to_le_bytes());
        out[4..12].copy_from_slice(&self.token.to_le_bytes());
        out
    }
}

impl Epolls {
    /// The descriptor an instance is really kept under, following one alias.
    fn resolve(&self, node: NodeId, fd: i64) -> i64 {
        self.aliases.get(&(node, fd)).copied().unwrap_or(fd)
    }

    /// Copies a descriptor, answering with the new one, or nothing where it is not ours.
    ///
    /// Both kinds are copied and both for the same reason. A runtime duplicates its poller so the
    /// thread that wakes it holds a handle of its own, and it duplicates the counter it wakes it
    /// *with* for exactly the same reason — a copy that named a counter of its own would be a
    /// second bell, and ringing it would wake nobody.
    pub(crate) fn dup(&mut self, node: NodeId, fd: i64) -> Option<i64> {
        let original = self.resolve(node, fd);
        let next = if self.instances.contains_key(&(node, original)) {
            self.next_epoll.entry(node).or_insert(EPOLL_FD_BASE)
        } else if self.counters.contains_key(&(node, original)) {
            self.next_counter.entry(node).or_insert(EVENTFD_BASE)
        } else {
            return None;
        };
        let copy = *next;
        *next += 1;
        self.aliases.insert((node, copy), original);
        Some(copy)
    }

    /// Opens an instance for one `poll` to wait on, numbered where no guest descriptor is.
    pub(crate) fn create_hidden(&mut self, node: NodeId) -> i64 {
        let next = self.next_hidden.entry(node).or_insert(HIDDEN_BASE);
        let id = *next;
        *next += 1;
        self.instances.insert((node, id), Instance::default());
        id
    }

    /// Opens an instance and answers with the descriptor naming it.
    pub(crate) fn create(&mut self, node: NodeId) -> i64 {
        let next = self.next_epoll.entry(node).or_insert(EPOLL_FD_BASE);
        let fd = *next;
        *next += 1;
        self.instances.insert((node, fd), Instance::default());
        fd
    }

    /// Opens an event counter with a starting value.
    pub(crate) fn open_counter(&mut self, node: NodeId, initial: u64) -> i64 {
        let next = self.next_counter.entry(node).or_insert(EVENTFD_BASE);
        let fd = *next;
        *next += 1;
        self.counters.insert((node, fd), initial);
        fd
    }

    /// Whether a descriptor is an event counter, following an alias to reach one.
    pub(crate) fn is_counter(&self, node: NodeId, fd: i64) -> bool {
        self.counters.contains_key(&(node, self.resolve(node, fd)))
    }

    /// Whether a node holds any poller or counter.
    pub(crate) fn node_holds_any(&self, node: NodeId) -> bool {
        self.instances.keys().chain(self.counters.keys()).any(|(held, _)| *held == node)
    }

    /// Whether this is a descriptor these tables issued: a poller, a counter, or a copy of one.
    pub(crate) fn holds(&self, node: NodeId, fd: i64) -> bool {
        let at = (node, self.resolve(node, fd));
        self.instances.contains_key(&at) || self.counters.contains_key(&at)
    }

    /// Adds, changes or removes a watch. False where the instance is not one of ours.
    pub(crate) fn control(
        &mut self,
        node: NodeId,
        epfd: i64,
        op: u64,
        fd: i64,
        watch: (u32, u64),
    ) -> bool {
        let epfd = self.resolve(node, epfd);
        let Some(instance) = self.instances.get_mut(&(node, epfd)) else {
            return false;
        };
        match op {
            CTL_ADD | CTL_MOD => {
                instance.watched.insert(
                    fd,
                    Watch { events: watch.0, token: watch.1, seen: None, fired: false },
                );
            }
            CTL_DEL => {
                instance.watched.remove(&fd);
            }
            _ => return false,
        }
        true
    }

    /// Everything an instance may report now, as `(descriptor, wanted events, token)`.
    ///
    /// An edge-triggered watch already reported in this epoch is left out, and so is a one-shot
    /// watch that has fired: reporting either again is a level a guest that asked for an edge
    /// spins on.
    pub(crate) fn watched(&self, node: NodeId, epfd: i64) -> Vec<(i64, u32, u64)> {
        self.instances.get(&(node, self.resolve(node, epfd))).map_or_else(Vec::new, |instance| {
            instance
                .watched
                .iter()
                .filter(|(_, w)| !w.fired)
                .filter(|(_, w)| w.events & EPOLLET == 0 || w.seen != Some(self.epoch))
                .map(|(fd, w)| (*fd, w.events, w.token))
                .collect()
        })
    }

    /// Records that these descriptors were reported to the guest from an instance.
    pub(crate) fn reported(&mut self, node: NodeId, epfd: i64, fds: &[i64]) {
        let epoch = self.epoch;
        let epfd = self.resolve(node, epfd);
        if let Some(instance) = self.instances.get_mut(&(node, epfd)) {
            for fd in fds {
                if let Some(watch) = instance.watched.get_mut(fd) {
                    watch.seen = Some(epoch);
                    if watch.events & EPOLLONESHOT != 0 {
                        watch.fired = true;
                    }
                }
            }
        }
    }

    /// Opens a new readiness epoch: something may have become ready.
    pub(crate) fn bump(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Parks a task until something the instance watches becomes ready.
    pub(crate) fn park(
        &mut self,
        node: NodeId,
        epfd: i64,
        task: TaskId,
        out: GuestAddr,
        max: usize,
        poll: bool,
    ) {
        let epfd = self.resolve(node, epfd);
        if let Some(instance) = self.instances.get_mut(&(node, epfd)) {
            instance.parked.push_back(Parked { task, out, max, poll });
        }
    }

    /// Takes the longest-waiting task on an instance, where one is waiting.
    pub(crate) fn take_parked(&mut self, node: NodeId, epfd: i64) -> Option<Taken> {
        let epfd = self.resolve(node, epfd);
        let instance = self.instances.get_mut(&(node, epfd))?;
        instance.parked.pop_front().map(|p| (p.task, p.out, p.max, p.poll))
    }

    /// Takes one named task off an instance, where it is still waiting there.
    ///
    /// **A timeout belongs to one waiter, and taking the front one was not it.** With two tasks on
    /// one instance -- the ordinary shape of a threaded runtime -- the timer took whoever had
    /// waited longest, found it was somebody else, and put that task back with no output buffer
    /// and no count: a later wake then reported nothing ready to a wait that had asked for as much
    /// as would fit. Meanwhile the task whose timer it was stayed parked with its timer spent.
    pub(crate) fn take_parked_task(
        &mut self,
        node: NodeId,
        epfd: i64,
        task: TaskId,
    ) -> Option<Taken> {
        let epfd = self.resolve(node, epfd);
        let instance = self.instances.get_mut(&(node, epfd))?;
        let at = instance.parked.iter().position(|p| p.task == task)?;
        instance.parked.remove(at).map(|p| (p.task, p.out, p.max, p.poll))
    }

    /// Every instance with somebody waiting on it, wherever it is.
    pub(crate) fn parked_instances(&self) -> Vec<(NodeId, i64)> {
        self.instances
            .iter()
            .filter(|(_, instance)| !instance.parked.is_empty())
            .map(|((node, epfd), _)| (*node, *epfd))
            .collect()
    }

    /// What an event counter holds.
    pub(crate) fn counter(&self, node: NodeId, fd: i64) -> Option<u64> {
        self.counters.get(&(node, self.resolve(node, fd))).copied()
    }

    /// Adds to an event counter, saturating as the kernel's own does short of its maximum.
    pub(crate) fn add_to_counter(&mut self, node: NodeId, fd: i64, by: u64) -> bool {
        let at = self.resolve(node, fd);
        match self.counters.get_mut(&(node, at)) {
            Some(counter) => {
                *counter = counter.saturating_add(by);
                true
            }
            None => false,
        }
    }

    /// Empties an event counter and answers what it held.
    pub(crate) fn drain_counter(&mut self, node: NodeId, fd: i64) -> Option<u64> {
        let at = self.resolve(node, fd);
        let counter = self.counters.get_mut(&(node, at))?;
        Some(std::mem::take(counter))
    }

    /// Forgets a descriptor: an instance, a counter, or a watch on any instance of the node.
    pub(crate) fn close(&mut self, node: NodeId, fd: i64) -> bool {
        // A copy going away takes the copy and nothing else: the thing it named is still named by
        // whoever it was copied from, and closing the last handle onto a poller is not what a
        // runtime does when it drops a spare one.
        if self.aliases.remove(&(node, fd)).is_some() {
            return true;
        }
        if fd >= HIDDEN_BASE {
            return self.instances.remove(&(node, fd)).is_some();
        }
        let was_ours = self.instances.remove(&(node, fd)).is_some()
            || self.counters.remove(&(node, fd)).is_some();
        // A closed descriptor stops being watched, whether or not it was ours: the guest may close
        // a socket an instance was told about, and reporting readiness on it afterwards would name
        // a descriptor that no longer exists.
        for ((owner, _), instance) in &mut self.instances {
            if *owner == node {
                instance.watched.remove(&fd);
            }
        }
        was_ours
    }

    /// Forgets everything a node had, because the node is gone.
    pub(crate) fn drop_node(&mut self, node: NodeId) {
        self.instances.retain(|(owner, _), _| *owner != node);
        self.counters.retain(|(owner, _), _| *owner != node);
        self.aliases.retain(|(owner, _), _| *owner != node);
        self.next_epoll.remove(&node);
        self.next_counter.remove(&node);
    }

    /// Forgets every wait a task was parked in, because the task has ended.
    pub(crate) fn forget_task(&mut self, task: TaskId) {
        for instance in self.instances.values_mut() {
            instance.parked.retain(|p| p.task != task);
        }
        // A `poll`'s own instance exists for the task waiting on it, and goes with it.
        self.instances.retain(|(_, id), instance| *id < HIDDEN_BASE || !instance.parked.is_empty());
    }
}
