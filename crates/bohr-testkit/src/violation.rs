//! A cluster that promises something about itself, and a world that can break the promise.
//!
//! A guest that really fails, judged by the shipped oracle, for testing reduction.
//!
//! # Script shape
//!
//! Reduction changes only what the world did, so both guests stop as soon as the world misbehaves:
//! the asserter reports the broken promise and exits, and the responder blocks until a scheduled
//! fault ends it. The run's length then depends on when the world acts.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_types::{GuestAddr, NodeId, Tick};
use bohr_world::{NetworkConfig, NodeFaultKind, ScheduledNodeFault, ScheduledPartition};

use crate::{Preload, Scenario, sockaddr_for, sys};

/// The node that makes the promise.
pub const ASKER: NodeId = NodeId(0);
/// The node that keeps it, until the world stops letting it.
pub const ANSWERER: NodeId = NodeId(1);

/// `FIONBIO`: what makes an empty receive return at once.
const FIONBIO: u64 = 0x5421;
/// `O_CREAT`, as an ordinary guest passes it.
const O_CREAT: u64 = 0o100;
/// The first socket and the first `openat` descriptor, both deterministic.
const SOCK: u64 = 4096;
const RECORDS_FD: u64 = 3;

const ADDR_SELF: u64 = MEMORY_BASE;
const ADDR_PEER: u64 = MEMORY_BASE + 16;
const PAYLOAD: u64 = MEMORY_BASE + 64;
const NONBLOCKING: u64 = MEMORY_BASE + 96;
const SLEEP: u64 = MEMORY_BASE + 128;
const PATH: u64 = MEMORY_BASE + 256;
const RECORD: u64 = MEMORY_BASE + 512;
/// The responder's one slot; a blocking receive returns only once it is filled.
const ANSWER_SLOT: u64 = MEMORY_BASE + 1024;
/// One slot per round, since a script cannot clear memory.
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
/// Rounds set the run's length; ticks set how early it fails. Reduction moves only the ticks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PingPong {
    rounds: usize,
    cut_at: Tick,
    kill_at: Tick,
}

impl PingPong {
    /// An exchange of `rounds` rounds that the world never interferes with.
    ///
    /// Runs clean on its own; [`Self::cut_at`] breaks it and [`Self::kill_at`] ends the responder.
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
    /// Without it the blocked responder is a deadlock, which would hide the broken promise.
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
            // `EAGAIN` leaves the slot empty and falls through to the complaint.
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

        // Every round answered: skip the complaint.
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
    /// Blocking, so the responder records nothing once answers stop.
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
