//! The family layer: what the laws of a class read, and the watch that feeds them.
//!
//! A law is not a test of one project but a rule over the model of a family of systems. Each
//! family's records are turned into one vocabulary — `Ack` and `Read` for durability, `Decided`
//! and the claims and reaches of `Fact` for agreement — by a translator of its own (`chain`, `kv`,
//! `log`); the laws (`laws`) read only that vocabulary, so one law judges a chain, a store and a
//! consensus log alike. Every judgement is a function of the records so far and of the world's own
//! timer, so a finding replays, reduces and ablates like any other. See `docs/ARCHITECTURE.md`
//! §5.21.

mod chain;
mod kv;
mod laws;
mod ledger;
mod log;

use std::collections::BTreeMap;

use bohr_sdk_proto::json::Json;
use bohr_trace::NodeState;
use bohr_types::{NodeId, Tick};

/// The family a scenario's laws are judged over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    /// Nodes that agree on blocks of transactions, fed by the load's records.
    Chain,
    /// A store that acknowledges puts, deletes and batches, fed by its harness's records.
    Kv,
    /// A consensus log that commits entries by index under terms, fed by its replicas' records.
    Log,
}

impl Family {
    /// The name `[model] kind` gives it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Chain => "chain",
            Self::Kv => "kv",
            Self::Log => "log",
        }
    }
}

/// The `[model]` section: which family, and how long a restarted node has to answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModelConfig {
    /// The family the laws read.
    pub family: Family,
    /// How long a node restarted by the world has to answer before R1.4 is broken.
    pub ready_within: Tick,
    /// Whether R1.4 is judged at all: only where every fault the scenario arms is graded `real`,
    /// so that whatever landed was real too.
    pub restart_judged: bool,
    /// How long after the fault window closes the system has to move again and every replica to
    /// catch up, for R3.1 and R3.2.
    pub moves_within: Tick,
    /// When the last fault the scenario arms can land: the latest of its scheduled faults and
    /// heals and of `faults.disarm_after_ticks`, or the start where it arms none.
    pub window_close: Tick,
}

/// An acknowledgement, in the vocabulary every law reads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Ack {
    /// What was acknowledged: a key, or a transaction.
    pub(crate) subject: String,
    /// The value it now holds; `None` for a removal.
    pub(crate) value: Option<String>,
    /// The batch it was part of, where it was.
    pub(crate) batch: Option<u64>,
    /// Its order among the acknowledgements and the operations begun.
    pub(crate) seq: u64,
    /// Where the guest made it, as `file:line`, where the record says.
    pub(crate) at: Option<String>,
}

/// A read of a subject, in the vocabulary every law reads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Read {
    /// What was read.
    pub(crate) subject: String,
    /// The value read; `None` where the subject was absent.
    pub(crate) value: Option<String>,
    /// The node that answered.
    pub(crate) node: u32,
    /// Which start of that node answered.
    pub(crate) start: u64,
    /// Whether the reader is past the acknowledgement and owes it: a chain node below the height a
    /// transaction was committed at is not.
    pub(crate) owes: bool,
}

/// A position decided by a replica, in the vocabulary every law of agreement reads: a block at a
/// height, an entry at an index.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Decided {
    /// The height or the index.
    pub(crate) position: u64,
    /// What was decided there: a block hash, an entry.
    pub(crate) value: String,
    /// The state deciding it left behind, where the family reports one.
    pub(crate) state: Option<String>,
    /// The replica that decided it.
    pub(crate) node: u32,
    /// Whether the replica reported it final.
    pub(crate) final_: bool,
}

/// What a translator makes of one record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Fact {
    /// An operation begun and not yet acknowledged, which may land or not.
    Begun(Ack),
    /// An operation acknowledged.
    Acked(Ack),
    /// A read.
    Read(Read),
    /// The node answered its first question since its last start.
    Ready { node: u32 },
    /// A replica decided a position.
    Decided(Decided),
    /// A node claimed a round: led a term, proposed at a height.
    Claimed { round: u64, node: u32 },
    /// A replica has come as far as a position: its head, its applied index.
    Reached { node: u32, position: u64 },
    /// An operation on counters, begun or acknowledged.
    Counted(ledger::Counted),
    /// A node reported a subject at a position: a transaction at a height.
    Reported { node: u32, subject: String, position: u64 },
    /// A node holds these subjects at a position, and only these.
    Holds { node: u32, position: u64, subjects: Vec<String> },
}

/// A law broken: its code, the node it was broken on, and what was seen.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Broken {
    /// The law's diagnostic code.
    pub code: &'static str,
    /// The node the law was broken on.
    pub node: u32,
    /// The subject, what was seen, and where the lost acknowledgement was made.
    pub message: String,
}

/// The watch a scenario with `[model]` runs: records in, broken laws out.
#[derive(Debug)]
pub struct Watch {
    config: ModelConfig,
    laws: laws::Laws,
    agreement: laws::Agreement,
    ledger: ledger::Ledger,
    api: ledger::Api,
    chain: chain::Chain,
    /// The nodes the records named, which owe catching up once the window closes.
    nodes: std::collections::BTreeSet<u32>,
    /// The nodes the world has down now, which owe nothing until they are up again.
    down: std::collections::BTreeSet<u32>,
    /// The node the load runs on, the one node whose `load.*` records are read.
    load_node: Option<u32>,
    /// Each node's starts so far; the first is 1.
    starts: BTreeMap<u32, u64>,
    /// The start each node last answered in.
    ready: BTreeMap<u32, u64>,
    /// The starts R1.4 was already reported for, and those the world ended or froze before the
    /// bound, which owe nothing.
    late: std::collections::BTreeSet<(u32, u64)>,
}

impl Watch {
    /// A watch over `config`'s family, reading `load.*` only from `load_node`.
    #[must_use]
    pub fn new(config: ModelConfig, load_node: Option<u32>) -> Self {
        Self {
            config,
            laws: laws::Laws::default(),
            agreement: laws::Agreement::default(),
            ledger: ledger::Ledger::default(),
            api: ledger::Api::default(),
            chain: chain::Chain::default(),
            nodes: std::collections::BTreeSet::new(),
            down: std::collections::BTreeSet::new(),
            load_node,
            starts: BTreeMap::new(),
            ready: BTreeMap::new(),
            late: std::collections::BTreeSet::new(),
        }
    }

    /// Reads one record `from` a node wrote; every law it breaks.
    ///
    /// A record of another family, or a `load.*` record from a node other than the load, is
    /// passed over, so no node grades itself.
    pub fn record(&mut self, from: NodeId, name: &str, details: &Json) -> Vec<Broken> {
        let facts = match self.config.family {
            Family::Kv if name.starts_with("kv.") => {
                let start = self.start_of(from.get());
                kv::facts(from.get(), start, name, details)
            }
            Family::Chain if name.starts_with("load.") && self.load_node == Some(from.get()) => {
                let starts = &self.starts;
                self.chain.facts(name, details, |node| starts.get(&node).copied().unwrap_or(1))
            }
            Family::Log if name.starts_with("log.") => log::facts(from.get(), name, details),
            _ => return Vec::new(),
        };
        let mut broken = Vec::new();
        for fact in facts {
            match fact {
                Fact::Begun(ack) => {
                    // A number put and not yet acknowledged leaves a counter's base unknown.
                    if ack.value.as_deref().is_some_and(|v| v.parse::<i64>().is_ok()) {
                        self.ledger.put_begun(&ack.subject);
                    }
                    self.laws.begun(ack);
                }
                Fact::Acked(ack) => {
                    // An acknowledged number, alone or in a batch, is a counter's base.
                    if let Some(n) = ack.value.as_deref().and_then(|v| v.parse::<i64>().ok()) {
                        self.ledger.base(&ack.subject, n);
                    }
                    self.laws.acked(ack);
                }
                Fact::Read(read) => {
                    broken.extend(self.laws.read(&read));
                    if self.ledger.knows(&read.subject) {
                        broken.extend(self.ledger.read(&read));
                    }
                }
                Fact::Counted(op) => self.ledger.counted(&op),
                Fact::Reported { node, subject, position } => {
                    broken.extend(self.api.reported(node, &subject, position));
                }
                Fact::Holds { node, position, subjects } => {
                    broken.extend(self.api.held(node, position, subjects));
                }
                Fact::Ready { node } => {
                    let start = self.start_of(node);
                    self.ready.insert(node, start);
                }
                Fact::Decided(decided) => {
                    self.nodes.insert(decided.node);
                    broken.extend(self.agreement.decided(&decided));
                }
                Fact::Claimed { round, node } => {
                    self.nodes.insert(node);
                    broken.extend(self.agreement.claimed(round, node));
                }
                Fact::Reached { node, position } => {
                    self.nodes.insert(node);
                    self.agreement.reached(node, position);
                }
            }
        }
        broken
    }

    /// Tells the watch what the world did to a node; where the world restarted it and R1.4 is
    /// judged, the moment by which it must have answered, and the start it must answer in.
    pub fn node_became(
        &mut self,
        node: NodeId,
        state: NodeState,
        now: Tick,
    ) -> Option<(Tick, u64)> {
        if Some(node.get()) == self.load_node {
            return None;
        }
        if matches!(state, NodeState::Dead | NodeState::Frozen) {
            // Put down again by the world: the start owes no answer, and the node no catching up.
            self.late.insert((node.get(), self.start_of(node.get())));
            self.down.insert(node.get());
            return None;
        }
        self.down.remove(&node.get());
        if state != NodeState::Restarting {
            return None;
        }
        let start = self.starts.entry(node.get()).or_insert(1);
        *start += 1;
        let start = *start;
        self.config
            .restart_judged
            .then(|| (Tick(now.get().saturating_add(self.config.ready_within.get())), start))
    }

    /// R1.4 at the moment a restarted node had to have answered by.
    pub fn ready_due(&mut self, node: NodeId, start: u64) -> Option<Broken> {
        let answered = self.ready.get(&node.get()).is_some_and(|seen| *seen >= start);
        let current = self.start_of(node.get()) == start;
        if answered || !current || !self.late.insert((node.get(), start)) {
            return None;
        }
        Some(Broken {
            code: bohr_trace::RESTARTS_CODE,
            node: node.get(),
            message: format!(
                "R1.4: node {} did not answer within {} ticks of its restart (start {start})",
                node.get(),
                self.config.ready_within.get()
            ),
        })
    }

    /// R1.4 for a node whose process ended on its own: one the world restarted that ends without
    /// having answered in this start never will.
    pub fn node_ended(&mut self, node: NodeId) -> Option<Broken> {
        // Its process gone, the node owes no catching up either.
        self.down.insert(node.get());
        let start = self.start_of(node.get());
        if !self.config.restart_judged || start < 2 || Some(node.get()) == self.load_node {
            return None;
        }
        self.ready_due(node, start)
    }

    /// When the fault window closes and when R3.1 and R3.2 are judged after it.
    #[must_use]
    pub const fn window(&self) -> (Tick, Tick) {
        let close = self.config.window_close;
        (close, Tick(close.get().saturating_add(self.config.moves_within.get())))
    }

    /// The fault window closed: what was decided by now is what the system must move past.
    pub fn window_closed(&mut self) {
        self.agreement.window_closed();
    }

    /// R3.1 and R3.2, the bound after the window closed. A chain decides a block every interval,
    /// so it must have moved; a log with nothing to commit need not, and is judged for catching up
    /// alone. A node the world has down owes nothing.
    pub fn window_due(&mut self) -> Vec<Broken> {
        // A torn crash, a restore from an older snapshot or a corrupted packet can leave a node
        // rightly unable to start, and the system rightly stopped with it.
        if !self.config.restart_judged {
            return Vec::new();
        }
        let owing: Vec<u32> = self
            .nodes
            .iter()
            .copied()
            .filter(|n| !self.down.contains(n) && Some(*n) != self.load_node)
            .collect();
        // A chain moves only with more than two thirds of its replicas up.
        let quorum = owing.len() * 3 > self.nodes.len() * 2;
        let moves = self.config.family == Family::Chain && quorum && !owing.is_empty();
        self.agreement.window_due(&owing, self.config.moves_within.get(), moves)
    }

    fn start_of(&self, node: u32) -> u64 {
        self.starts.get(&node).copied().unwrap_or(1)
    }
}
