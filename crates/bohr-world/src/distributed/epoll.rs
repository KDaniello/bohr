//! Readiness: which descriptors have something to offer, and who is waiting to hear it.
//!
//! Asynchronous runtimes are built on `epoll`; it asks about many descriptors the model already
//! tracks.
//!
//! It lives in the cluster model, the only place that sees both files and sockets.

use std::collections::{BTreeMap, VecDeque};

use bohr_types::{GuestAddr, NodeId, TaskId};

use super::{FIRST_DESCRIPTOR, Numbers};

/// First identity an instance or a counter is kept under, past every number a guest's descriptor
/// can hold.
///
/// A guest names one by a number from its process's one table; it is kept under an identity of
/// its own, so a `dup` or a close of one number never moves it.
const ID_BASE: i64 = 1 << 32;

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
/// Report a descriptor once, when it becomes ready.
pub(crate) const EPOLLET: u32 = 1 << 31;
/// Report a descriptor once, and then not again until it is modified.
pub(crate) const EPOLLONESHOT: u32 = 1 << 30;

/// Size of `struct epoll_event` on x86-64, which is packed: a `u32` and a `u64` with no padding.
pub(crate) const EVENT_BYTES: usize = 12;

/// How often a descriptor has had something arrive, and how often a write found it full.
///
/// Each only grows. An edge-triggered watch reports when one has moved since its last report:
/// readable once more arrived, writable once room came back after a write found none, which is the
/// edge from full to not full.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Edges {
    /// Bytes, a message, a datagram, a connection, the end of a stream or an error.
    pub(crate) arrived: u64,
    /// A write that did not go whole.
    pub(crate) filled: u64,
}

/// What one watched descriptor asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Watch {
    /// The events the guest wants to hear about.
    events: u32,
    /// The guest's token, returned untouched.
    token: u64,
    /// The descriptor's edges when it was last reported, for an edge-triggered watch.
    seen: Option<Edges>,
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
    /// Whether the answer is `pollfd` entries.
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
    /// A queue, since several threads may wait on one instance.
    parked: VecDeque<Parked>,
}

/// An event counter, as `eventfd2` makes one.
#[derive(Debug, Default)]
struct Counter {
    value: u64,
    /// `EFD_SEMAPHORE`: a read takes one, not the whole value.
    semaphore: bool,
    /// `EFD_NONBLOCK`: a read of nought answers `EAGAIN` instead of waiting.
    nonblocking: bool,
    /// Tasks parked in a read of nought, oldest first, and where each wants the value.
    readers: VecDeque<(TaskId, GuestAddr)>,
    edges: Edges,
}

impl Counter {
    /// Takes what a read takes: one under `EFD_SEMAPHORE`, else the whole value.
    fn take(&mut self) -> u64 {
        let taken = if self.semaphore { 1 } else { self.value };
        self.value -= taken;
        taken
    }
}

/// A read of a counter: the value it took, or that it waits.
pub(crate) enum CounterRead {
    Took(u64),
    WouldBlock,
    Waits,
}

/// The readiness calls this part answers.
pub(crate) const HANDLES: &[u32] = &[
    super::poll_sys::EPOLL_CREATE1,
    super::poll_sys::EPOLL_CTL,
    super::poll_sys::EPOLL_WAIT,
    super::poll_sys::EPOLL_PWAIT,
    super::poll_sys::EVENTFD2,
];

/// What a node was asked to report readiness on.
#[derive(Debug, Default)]
pub(crate) struct Epolls {
    instances: BTreeMap<(NodeId, i64), Instance>,
    /// Event counters, by identity.
    ///
    /// Runtimes use them to wake their poller.
    counters: BTreeMap<(NodeId, i64), Counter>,
    /// Every descriptor a guest holds on an instance or a counter, and the identity it names.
    ///
    /// A `dup` adds a second number for one; it goes with its last number.
    descriptors: BTreeMap<(NodeId, i64), i64>,
    /// The next identity; never reused.
    next_id: i64,
    /// The next number for an instance a `poll` makes for itself.
    next_hidden: BTreeMap<NodeId, i64>,
}

/// Past every guest descriptor, so closing a `poll`'s instance never closes a guest's.
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
    /// The descriptor's edges when this was read, kept as what the watch last saw.
    pub(crate) edges: Edges,
}

/// One watch an instance holds, as the world reads readiness for it.
pub(crate) struct Watching {
    pub(crate) fd: i64,
    pub(crate) events: u32,
    pub(crate) token: u64,
    /// The edges it was last reported at; `None` before its first report.
    pub(crate) seen: Option<Edges>,
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
    /// The identity a descriptor names.
    ///
    /// An identity passes through, so internal callers may hand one in; a number no instance or
    /// counter descriptor holds answers -1, which names nothing.
    fn resolve(&self, node: NodeId, fd: i64) -> i64 {
        if fd >= ID_BASE {
            return fd;
        }
        self.descriptors.get(&(node, fd)).copied().unwrap_or(-1)
    }

    /// Copies a descriptor to the lowest free number at or above `at_least`, answering with it,
    /// or nothing where it is not ours.
    pub(crate) fn dup(
        &mut self,
        node: NodeId,
        fd: i64,
        at_least: i64,
        numbers: &mut Numbers<'_>,
    ) -> Option<i64> {
        let id = self.resolve(node, fd);
        if !self.instances.contains_key(&(node, id)) && !self.counters.contains_key(&(node, id)) {
            return None;
        }
        let copy = numbers.claim(node, at_least);
        self.descriptors.insert((node, copy), id);
        Some(copy)
    }

    /// Copies a descriptor onto exactly `onto`, as `dup2` does; the caller has closed whatever
    /// `onto` held. False where `fd` is not ours.
    pub(crate) fn dup_onto(
        &mut self,
        node: NodeId,
        fd: i64,
        onto: i64,
        numbers: &mut Numbers<'_>,
    ) -> bool {
        if !self.holds(node, fd) {
            return false;
        }
        let id = self.resolve(node, fd);
        numbers.claim_exact(node, onto);
        self.descriptors.insert((node, onto), id);
        true
    }

    /// A fresh identity, given the lowest free number in its process's table.
    fn named(&mut self, node: NodeId, numbers: &mut Numbers<'_>) -> (i64, i64) {
        self.next_id = self.next_id.max(ID_BASE);
        let id = self.next_id;
        self.next_id += 1;
        let fd = numbers.claim(node, FIRST_DESCRIPTOR);
        self.descriptors.insert((node, fd), id);
        (id, fd)
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
    pub(crate) fn create(&mut self, node: NodeId, numbers: &mut Numbers<'_>) -> i64 {
        let (id, fd) = self.named(node, numbers);
        self.instances.insert((node, id), Instance::default());
        fd
    }

    /// Opens an event counter with a starting value, a semaphore under `EFD_SEMAPHORE` and
    /// non-blocking under `EFD_NONBLOCK`.
    pub(crate) fn open_counter(
        &mut self,
        node: NodeId,
        initial: u64,
        flags: u64,
        numbers: &mut Numbers<'_>,
    ) -> i64 {
        const EFD_SEMAPHORE: u64 = 1;
        const EFD_NONBLOCK: u64 = 0o4000;
        let (id, fd) = self.named(node, numbers);
        let counter = Counter {
            value: initial,
            semaphore: flags & EFD_SEMAPHORE != 0,
            nonblocking: flags & EFD_NONBLOCK != 0,
            readers: VecDeque::new(),
            edges: Edges::default(),
        };
        self.counters.insert((node, id), counter);
        fd
    }

    /// Whether a counter answers `EAGAIN` instead of waiting, or nothing where it is not one.
    pub(crate) fn counter_nonblocking(&self, node: NodeId, fd: i64) -> Option<bool> {
        self.counters.get(&(node, self.resolve(node, fd))).map(|c| c.nonblocking)
    }

    /// Sets or clears a counter's non-blocking flag, as `F_SETFL` does.
    pub(crate) fn set_counter_nonblocking(&mut self, node: NodeId, fd: i64, on: bool) {
        let at = (node, self.resolve(node, fd));
        if let Some(counter) = self.counters.get_mut(&at) {
            counter.nonblocking = on;
        }
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

    /// Every watch an instance may report from, leaving out one-shot watches that fired.
    pub(crate) fn watched(&self, node: NodeId, epfd: i64) -> Vec<Watching> {
        self.instances.get(&(node, self.resolve(node, epfd))).map_or_else(Vec::new, |instance| {
            instance
                .watched
                .iter()
                .filter(|(_, w)| !w.fired)
                .map(|(fd, w)| Watching { fd: *fd, events: w.events, token: w.token, seen: w.seen })
                .collect()
        })
    }

    /// Records that these were reported to the guest from an instance, at the edges they were
    /// read at.
    pub(crate) fn reported(&mut self, node: NodeId, epfd: i64, ready: &[Ready]) {
        let epfd = self.resolve(node, epfd);
        if let Some(instance) = self.instances.get_mut(&(node, epfd)) {
            for one in ready {
                if let Some(watch) = instance.watched.get_mut(&one.fd) {
                    watch.seen = Some(one.edges);
                    if watch.events & EPOLLONESHOT != 0 {
                        watch.fired = true;
                    }
                }
            }
        }
    }

    /// A counter's edges, or nothing where the descriptor is not a counter.
    pub(crate) fn counter_edges(&self, node: NodeId, fd: i64) -> Option<Edges> {
        self.counters.get(&(node, self.resolve(node, fd))).map(|c| c.edges)
    }

    /// Notes that a write found a counter with no room for what it added.
    pub(crate) fn counter_full(&mut self, node: NodeId, fd: i64) {
        if let Some(counter) = self.counters.get_mut(&(node, self.resolve(node, fd))) {
            counter.edges.filled += 1;
        }
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
    /// For a timeout, which belongs to one named waiter.
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
        self.counters.get(&(node, self.resolve(node, fd))).map(|c| c.value)
    }

    /// Adds to an event counter and hands the readers parked on it what each takes, writing the
    /// value where each asked for it. False where `fd` is not a counter.
    ///
    /// The value is held below `u64::MAX`; an addition that would pass it is refused by the
    /// caller first, see [`Self::counter_room`].
    pub(crate) fn add_to_counter(
        &mut self,
        node: NodeId,
        fd: i64,
        by: u64,
        mut hand: impl FnMut(TaskId, GuestAddr, u64) -> Result<(), bohr_kernel::KernelError>,
    ) -> Result<bool, bohr_kernel::KernelError> {
        let at = (node, self.resolve(node, fd));
        let Some(counter) = self.counters.get_mut(&at) else {
            return Ok(false);
        };
        counter.value = counter.value.saturating_add(by);
        if by > 0 {
            counter.edges.arrived += 1;
        }
        while counter.value > 0 {
            let Some((task, out)) = counter.readers.pop_front() else { break };
            let taken = counter.take();
            hand(task, out, taken)?;
        }
        Ok(true)
    }

    /// How much more a counter takes before it would reach `u64::MAX`, which Linux never lets
    /// it hold.
    pub(crate) fn counter_room(&self, node: NodeId, fd: i64) -> Option<u64> {
        let counter = self.counters.get(&(node, self.resolve(node, fd)))?;
        Some(u64::MAX - 1 - counter.value)
    }

    /// Reads a counter for `task`, which wants the value at `out`.
    pub(crate) fn read_counter(
        &mut self,
        node: NodeId,
        fd: i64,
        task: TaskId,
        out: GuestAddr,
    ) -> Option<CounterRead> {
        let counter = self.counters.get_mut(&(node, self.resolve(node, fd)))?;
        if counter.value > 0 {
            return Some(CounterRead::Took(counter.take()));
        }
        if counter.nonblocking {
            return Some(CounterRead::WouldBlock);
        }
        counter.readers.push_back((task, out));
        Some(CounterRead::Waits)
    }

    /// Closes a descriptor, answering whether it was one of ours, and forgets every watch on its
    /// number, ours or not.
    ///
    /// The instance or counter goes once no other descriptor names it. A closed number is
    /// watched no more, so a later descriptor that takes the number starts unwatched.
    pub(crate) fn close(&mut self, node: NodeId, fd: i64, numbers: &mut Numbers<'_>) -> bool {
        if fd >= HIDDEN_BASE {
            return self.instances.remove(&(node, fd)).is_some();
        }
        for ((owner, _), instance) in &mut self.instances {
            if *owner == node {
                instance.watched.remove(&fd);
            }
        }
        let Some(id) = self.descriptors.remove(&(node, fd)) else {
            return false;
        };
        numbers.release(node, fd);
        let still_named =
            self.descriptors.iter().any(|((at, _), named)| *at == node && *named == id);
        if !still_named {
            self.instances.remove(&(node, id));
            self.counters.remove(&(node, id));
        }
        true
    }

    /// Forgets everything a node had, because the node is gone.
    pub(crate) fn drop_node(&mut self, node: NodeId) {
        self.instances.retain(|(owner, _), _| *owner != node);
        self.counters.retain(|(owner, _), _| *owner != node);
        self.descriptors.retain(|(owner, _), _| *owner != node);
    }

    /// Forgets every wait a task was parked in, because the task has ended.
    pub(crate) fn forget_task(&mut self, task: TaskId) {
        for instance in self.instances.values_mut() {
            instance.parked.retain(|p| p.task != task);
        }
        for counter in self.counters.values_mut() {
            counter.readers.retain(|(reader, _)| *reader != task);
        }
        // A `poll`'s instance goes with its task.
        self.instances.retain(|(_, id), instance| *id < HIDDEN_BASE || !instance.parked.is_empty());
    }
}
