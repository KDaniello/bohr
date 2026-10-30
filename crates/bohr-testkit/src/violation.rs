//! A cluster that promises something about itself, and a world that can break the promise.
//!
//! Everything else in this crate is scripted to test *reproduction*: the fixtures run clean and
//! what is measured is whether two runs agree. Reduction asks the opposite question — what is the
//! smallest scenario that still goes wrong — so it needs a guest that genuinely goes wrong, judged
//! by the oracle the tool ships rather than by one with no opinion.
//!
//! # Why the scripts are shaped as they are
//!
//! A script cannot be shortened. Reduction removes what the *world* did — a partition, a fault, a
//! delivery — and the program it was done to is whatever it was compiled as. So a guest that keeps
//! polling after the answer stops coming would record its remaining rounds whatever a reducer took
//! away, and the run could never become small. Both guests therefore stop as soon as the world
//! stops behaving: the asserter says its promise was broken and exits, and the responder is
//! blocked on a receive that a scheduled fault ends. That is what makes the difference between the
//! long run and the short one a matter of *when* the world acts rather than of how much guest is
//! left to execute.

use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
use bohr_types::{GuestAddr, NodeId, Tick};
use bohr_world::{NetworkConfig, NodeFaultKind, ScheduledNodeFault, ScheduledPartition};

use crate::{sockaddr_for, sys, Preload, Scenario};

/// The node that makes the promise.
pub const ASKER: NodeId = NodeId(0);
/// The node that keeps it, until the world stops letting it.
pub const ANSWERER: NodeId = NodeId(1);

/// `FIONBIO`: what makes an empty receive answer rather than park.
const FIONBIO: u64 = 0x5421;
/// `O_CREAT`, as an ordinary guest passes it.
const O_CREAT: u64 = 0o100;
/// The first socket a node opens, and the first descriptor `openat` hands out. Both deterministic,
/// which is what lets a script name them without having read a return value.
const SOCK: u64 = 4096;
const RECORDS_FD: u64 = 3;

const ADDR_SELF: u64 = MEMORY_BASE;
const ADDR_PEER: u64 = MEMORY_BASE + 16;
const PAYLOAD: u64 = MEMORY_BASE + 64;
const NONBLOCKING: u64 = MEMORY_BASE + 96;
const SLEEP: u64 = MEMORY_BASE + 128;
const PATH: u64 = MEMORY_BASE + 256;
const RECORD: u64 = MEMORY_BASE + 512;
/// The responder reads into one slot: a blocking receive only returns when something filled it,
/// so there is no stale answer to tell apart from a fresh one.
const ANSWER_SLOT: u64 = MEMORY_BASE + 1024;
/// The asserter needs one slot per round. Nothing in a script can clear memory, so a reused slot
/// would still hold the answer to the round before and every later round would look answered.
const INBOX: u64 = MEMORY_BASE + 4096;

/// Eight bytes each way, so a script can branch on what arrived.
const PING: u64 = 0x0067_6E69_705F_6862;
const PONG: u64 = 0x0067_6E6F_705F_6862;

/// The path the world recognises as the record channel.
const CHANNEL: &[u8] = b"/bohr/records.jsonl\0";

/// What the asserter claims about itself, in the words the record carries.
pub const PROMISE: &str = "every ping is answered";

/// The record an SDK writes when an `always` does not hold.
const FAILED: &str = "{\"antithesis_assert\":{\"hit\":true,\"must_hit\":true,\"assert_type\":\"always\",\"display_type\":\"Always\",\"message\":\"every ping is answered\",\"condition\":false,\"id\":\"every ping is answered\",\"location\":{\"file\":\"src/ping.rs\",\"function\":\"round\",\"begin_line\":214,\"begin_column\":9}},\"bohr\":{\"schema\":1,\"seq\":1,\"node\":0}}\n";

/// How long the asserter waits for an answer before deciding there is none.
const PATIENCE_TICKS: u64 = 2_000_000;

/// A two-node exchange, and the two moments the world can break it at.
///
/// The rounds decide how long the run is; the ticks decide how early it goes wrong. A reduction
/// works on the second pair and never on the first, which is the whole point of the fixture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PingPong {
    rounds: usize,
    cut_at: Tick,
    kill_at: Tick,
}

impl PingPong {
    /// An exchange of `rounds` rounds that the world never interferes with.
    ///
    /// On its own this runs clean: every ping is answered and the guest exits without complaining.
    /// A caller breaks it with [`Self::cut_at`] and ends the responder with [`Self::kill_at`].
    #[must_use]
    pub const fn new(rounds: usize) -> Self {
        Self { rounds, cut_at: Tick(u64::MAX), kill_at: Tick(u64::MAX) }
    }

    /// Cuts the link both ways at this tick, so no answer arrives after it.
    #[must_use]
    pub const fn cut_at(mut self, at: Tick) -> Self {
        self.cut_at = at;
        self
    }

    /// Ends the responder at this tick.
    ///
    /// Needed rather than tidy: the responder waits on a receive, and a task waiting with nothing
    /// left to release it is a deadlock — which the kernel reports as a hang, not as the broken
    /// promise the fixture is about. The fault is what lets the run end on the verdict it earned.
    #[must_use]
    pub const fn kill_at(mut self, at: Tick) -> Self {
        self.kill_at = at;
        self
    }

    /// How many rounds the asserter is prepared to run.
    #[must_use]
    pub const fn rounds(&self) -> usize {
        self.rounds
    }

    /// The programs, one per node.
    #[must_use]
    pub fn programs(&self) -> Vec<NullProgram> {
        vec![self.asks(), self.answers()]
    }

    /// What both nodes need in memory before they start.
    #[must_use]
    pub fn preloads(&self) -> Vec<Preload> {
        let mut timespec = vec![0u8; 16];
        timespec[8..16].copy_from_slice(&PATIENCE_TICKS.to_le_bytes());
        let on = 1u32.to_le_bytes().to_vec();

        vec![
            (ASKER.get(), ADDR_SELF, sockaddr_for(ASKER.get(), 9000)),
            (ASKER.get(), ADDR_PEER, sockaddr_for(ANSWERER.get(), 7000)),
            (ASKER.get(), PAYLOAD, PING.to_le_bytes().to_vec()),
            (ASKER.get(), NONBLOCKING, on),
            (ASKER.get(), SLEEP, timespec),
            (ASKER.get(), PATH, CHANNEL.to_vec()),
            (ASKER.get(), RECORD, FAILED.as_bytes().to_vec()),
            (ANSWERER.get(), ADDR_SELF, sockaddr_for(ANSWERER.get(), 7000)),
            (ANSWERER.get(), ADDR_PEER, sockaddr_for(ASKER.get(), 9000)),
            (ANSWERER.get(), PAYLOAD, PONG.to_le_bytes().to_vec()),
        ]
    }

    /// The world the exchange runs in.
    #[must_use]
    pub fn scenario(&self) -> Scenario {
        let mut network =
            NetworkConfig { nodes: 2, latency_mean_ticks: 100_000, ..NetworkConfig::default() };
        if self.cut_at != Tick(u64::MAX) {
            network.partitions.push(ScheduledPartition {
                at_tick: self.cut_at,
                heal_at_tick: Tick(u64::MAX),
                cut: vec![(ASKER, ANSWERER), (ANSWERER, ASKER)],
            });
        }
        let mut node_faults = Vec::new();
        if self.kill_at != Tick(u64::MAX) {
            node_faults.push(ScheduledNodeFault {
                at_tick: self.kill_at,
                node: ANSWERER,
                kind: NodeFaultKind::Kill,
            });
        }
        Scenario {
            network,
            node_faults,
            sdk_output: Some(String::from_utf8_lossy(&CHANNEL[..CHANNEL.len() - 1]).into_owned()),
            ..Scenario::default()
        }
    }

    /// Ping, wait, and say so the first time no answer is there.
    fn asks(&self) -> NullProgram {
        const HEAD: usize = 4;
        const PER_ROUND: usize = 5;
        let complain_at = HEAD + self.rounds * PER_ROUND;

        let mut ops = vec![
            GuestOp::Syscall { nr: sys::SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: sys::BIND, args: [SOCK, ADDR_SELF, 16, 0, 0, 0] },
            GuestOp::Syscall { nr: sys::IOCTL, args: [SOCK, FIONBIO, NONBLOCKING, 0, 0, 0] },
            GuestOp::Syscall { nr: sys::SENDTO, args: [SOCK, PAYLOAD, 8, 0, ADDR_PEER, 16] },
        ];
        for round in 0..self.rounds {
            let slot = INBOX + 8 * round as u64;
            let base = HEAD + round * PER_ROUND;
            ops.push(GuestOp::Syscall { nr: sys::CLOCK_NANOSLEEP, args: [0, 0, SLEEP, 0, 0, 0] });
            ops.push(GuestOp::Syscall { nr: sys::RECVFROM, args: [SOCK, slot, 8, 0, 0, 0] });
            // An answer carries on; an empty queue answers `EAGAIN` and leaves the slot as it
            // was, which falls through to the complaint.
            ops.push(GuestOp::JumpIfMem { addr: GuestAddr(slot), equals: PONG, pc: base + 4 });
            ops.push(GuestOp::Jump { pc: complain_at + 1 });
            ops.push(GuestOp::Syscall {
                nr: sys::SENDTO,
                args: [SOCK, PAYLOAD, 8, 0, ADDR_PEER, 16],
            });
        }
        debug_assert_eq!(
            ops.len(),
            complain_at,
            "the rounds do not end where the complaint begins"
        );

        // Running out of rounds with every one answered is a guest that kept its promise: it
        // arrives here by falling through and steps over the complaint, which is reached only by
        // the jump above.
        ops.push(GuestOp::Jump { pc: complain_at + 3 });
        ops.push(GuestOp::Syscall { nr: sys::OPENAT, args: [0, PATH, O_CREAT, 0, 0, 0] });
        ops.push(GuestOp::Syscall {
            nr: sys::WRITE,
            args: [RECORDS_FD, RECORD, FAILED.len() as u64, 0, 0, 0],
        });
        ops.push(GuestOp::Exit(0));
        NullProgram::single(ops)
    }

    /// Answer whatever arrives, and wait when nothing has.
    ///
    /// The receive blocks rather than polling: a poll would need a sleep to be bearable, and every
    /// sleep is a round the responder records whether or not anything is happening. Blocking makes
    /// the responder cost nothing at all once the answers stop.
    fn answers(&self) -> NullProgram {
        let mut ops = vec![
            GuestOp::Syscall { nr: sys::SOCKET, args: [2, 2, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: sys::BIND, args: [SOCK, ADDR_SELF, 16, 0, 0, 0] },
        ];
        for _ in 0..self.rounds {
            ops.push(GuestOp::Syscall { nr: sys::RECVFROM, args: [SOCK, ANSWER_SLOT, 8, 0, 0, 0] });
            ops.push(GuestOp::Syscall {
                nr: sys::SENDTO,
                args: [SOCK, PAYLOAD, 8, 0, ADDR_PEER, 16],
            });
        }
        ops.push(GuestOp::Exit(0));
        NullProgram::single(ops)
    }
}
