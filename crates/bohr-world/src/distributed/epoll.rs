//! Readiness: which descriptors have something to offer, and who is waiting to hear it.
//!
//! Asynchronous runtimes are built on `epoll`; it asks about many descriptors the model already
//! tracks.
//!
//! It lives in the cluster model, the only place that sees both files and sockets.

use std::collections::{BTreeMap, VecDeque};

use bohr_types::{GuestAddr, NodeId, TaskId};

/// First descriptor handed out for an epoll instance.
///
/// A range clear of files (from 3) and sockets (from 4096), so descriptors are unambiguous.
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
/// Report a descriptor once, when it becomes ready.
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
    /// The guest's token, returned untouched.
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

/// What a node was asked to report readiness on.
#[derive(Debug, Default)]
pub(crate) struct Epolls {
    instances: BTreeMap<(NodeId, i64), Instance>,
    /// Event counters, by the descriptor each was opened as.
    ///
    /// Runtimes use them to wake their poller.
    counters: BTreeMap<(NodeId, i64), u64>,
    /// Duplicate descriptors, each naming the instance it was copied from.
    ///
    /// Aliases, since a `dup` names the same instance.
    aliases: BTreeMap<(NodeId, i64), i64>,
    next_epoll: BTreeMap<NodeId, i64>,
    next_counter: BTreeMap<NodeId, i64>,
    /// The next number for an instance a `poll` makes for itself.
    next_hidden: BTreeMap<NodeId, i64>,
    /// Counts moments readiness may change: deliveries, counter writes, heals. Edge-triggered
    /// watches report once per epoch.
    epoch: u64,
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
    /// Pollers and counters are both aliased.
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
    /// Leaves out edge-triggered watches already reported this epoch and fired one-shot watches.
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
        // Closing an alias removes only the alias.
        if self.aliases.remove(&(node, fd)).is_some() {
            return true;
        }
        if fd >= HIDDEN_BASE {
            return self.instances.remove(&(node, fd)).is_some();
        }
        let was_ours = self.instances.remove(&(node, fd)).is_some()
            || self.counters.remove(&(node, fd)).is_some();
        // A closed descriptor is no longer watched, ours or not.
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
        // A `poll`'s instance goes with its task.
        self.instances.retain(|(_, id), instance| *id < HIDDEN_BASE || !instance.parked.is_empty());
    }
}
