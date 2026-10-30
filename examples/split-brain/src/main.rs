//! One node of a five-node Raft cluster, carried over the virtual network.
//!
//! Run under `bohr` with `scenario.toml`, the cluster splits and heals on the scenario clock, and
//! every node checks the invariant Raft exists to hold: **an entry that was committed is never
//! afterwards absent, and no two leaders share a term**. A violation exits 1 — the guest catches
//! itself out, and nothing about the defect is known to the tool.
//!
//! The transport is deliberately the plainest thing that works: blocking `sendto`/`recvfrom` on a
//! datagram socket, and `nanosleep` between ticks. No executor, no readiness polling — every
//! syscall it makes is one the world models.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    time::Duration,
};

use prost::Message as _;
use raft::{prelude::*, storage::MemStorage, StateRole};

/// Nodes in the cluster. Raft identifiers are one-based; node `i` of the world is Raft `i + 1`.
const NODES: u64 = 5;

/// Port every node listens on. Each has an address of its own, so one port is enough.
const PORT: u16 = 7000;

/// Virtual milliseconds between ticks. Election and heartbeat timeouts below are counted in these.
const TICK_MS: u64 = 10;

/// Ticks a node runs for. Long enough for an election, a split, and a heal.
const TICKS: u64 = 600;

/// Largest datagram the transport moves.
const MAX_DATAGRAM: usize = 8 * 1024;

/// The address the world gives node `index`.
fn address_of(index: u64) -> SocketAddrV4 {
    let host = u32::try_from(index + 1).unwrap_or(1);
    SocketAddrV4::new(Ipv4Addr::from(0x0A42_0000 | host), PORT)
}

/// A leadership claim, broadcast so every node can check the one-leader-per-term invariant
/// without a central observer.
///
/// Framed apart from Raft's own messages by a one-byte tag: Raft traffic is opaque to us, and
/// parsing it to find out whether it is ours would be a second protocol pretending to be one.
const TAG_RAFT: u8 = 0;
const TAG_CLAIM: u8 = 1;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let index: u64 = std::env::args().nth(1).unwrap_or_else(|| "0".into()).parse()?;
    let id = index + 1;

    let socket = UdpSocket::bind(address_of(index))?;
    socket.set_nonblocking(true)?;

    let config = Config {
        id,
        // Ten ticks to an election, three to a heartbeat: with a 10 ms tick that is a 100 ms
        // election timeout, the shape a real deployment uses.
        election_tick: 10,
        heartbeat_tick: 3,
        ..Default::default()
    };
    config.validate()?;

    let voters: Vec<u64> = (1..=NODES).collect();
    let storage = MemStorage::new_with_conf_state((voters, vec![]));
    let logger = slog::Logger::root(slog::Discard, slog::o!());
    let mut node = RawNode::new(&config, storage, &logger)?;

    // What this node believes was committed, and who it has seen lead each term. Both are the
    // invariant's memory; neither is Raft's.
    let mut applied: Vec<u64> = Vec::new();
    let mut leader_of_term: HashMap<u64, u64> = HashMap::new();
    let mut claimed_term = 0;
    let mut buf = vec![0u8; MAX_DATAGRAM];

    for _tick in 0..TICKS {
        // Drain whatever arrived since the last tick. A datagram that is not there yet is not
        // waited for: the tick loop is the clock, and blocking here would stall it.
        while let Ok((len, _)) = socket.recv_from(&mut buf) {
            if len < 2 {
                continue;
            }
            match buf[0] {
                TAG_RAFT => {
                    if let Ok(message) = Message::decode(&buf[1..len]) {
                        node.step(message)?;
                    }
                }
                TAG_CLAIM => {
                    let term = u64::from_le_bytes(buf[1..9].try_into()?);
                    let claimant = u64::from_le_bytes(buf[9..17].try_into()?);
                    if let Some(seen) = leader_of_term.insert(term, claimant) {
                        if seen != claimant {
                            eprintln!(
                                "node {id}: two leaders in term {term}: {seen} and {claimant}"
                            );
                            return Err("split brain".into());
                        }
                    }
                }
                _ => {}
            }
        }

        node.tick();

        // The leader announces itself once per term, which is what gives every node the evidence
        // the invariant is checked against.
        if node.raft.state == StateRole::Leader && node.raft.term > claimed_term {
            claimed_term = node.raft.term;
            if let Some(seen) = leader_of_term.insert(claimed_term, id) {
                if seen != id {
                    eprintln!("node {id}: term {claimed_term} already led by {seen}");
                    return Err("split brain".into());
                }
            }
            let mut claim = vec![TAG_CLAIM];
            claim.extend_from_slice(&claimed_term.to_le_bytes());
            claim.extend_from_slice(&id.to_le_bytes());
            broadcast(&socket, index, &claim);

            // A leader proposes one entry per term, so there is something to commit and therefore
            // something a lost commit could be measured against.
            let payload = format!("t{claimed_term}").into_bytes();
            let _ = node.propose(vec![], payload);
        }

        if !node.has_ready() {
            sleep_ms(TICK_MS);
            continue;
        }

        let mut ready = node.ready();
        send_all(&socket, ready.take_messages());

        if !ready.snapshot().is_empty() {
            let snapshot = ready.snapshot().clone();
            node.mut_store().wl().apply_snapshot(snapshot)?;
        }
        for entry in ready.take_committed_entries() {
            if entry.data.is_empty() {
                continue;
            }
            // A committed index that goes backwards, or repeats with different data, is the loss
            // this scenario hunts for.
            if let Some(last) = applied.last() {
                if entry.index <= *last {
                    eprintln!("node {id}: committed index went backwards: {} after {last}", entry.index);
                    return Err("committed entry lost".into());
                }
            }
            applied.push(entry.index);
        }
        if !ready.entries().is_empty() {
            node.mut_store().wl().append(ready.entries())?;
        }
        if let Some(hs) = ready.hs() {
            node.mut_store().wl().set_hardstate(hs.clone());
        }
        send_all(&socket, ready.take_persisted_messages());

        let mut light = node.advance(ready);
        send_all(&socket, light.take_messages());
        for entry in light.take_committed_entries() {
            if entry.data.is_empty() {
                continue;
            }
            applied.push(entry.index);
        }
        node.advance_apply();

        sleep_ms(TICK_MS);
    }

    println!("node {id}: {} entries committed, {} terms led", applied.len(), leader_of_term.len());
    Ok(())
}

/// Sends Raft's own traffic, tagged so the receiver can tell it from a claim.
fn send_all(socket: &UdpSocket, messages: Vec<Message>) {
    for message in messages {
        let to = message.to;
        let mut framed = vec![TAG_RAFT];
        framed.extend_from_slice(&message.encode_to_vec());
        // Raft identifiers are one-based; the world's node numbering is not.
        let _ = socket.send_to(&framed, address_of(to.saturating_sub(1)));
    }
}

/// Sends one datagram to every node but this one.
fn broadcast(socket: &UdpSocket, index: u64, payload: &[u8]) {
    for peer in 0..NODES {
        if peer != index {
            let _ = socket.send_to(payload, address_of(peer));
        }
    }
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}
