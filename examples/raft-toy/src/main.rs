//! One node of a three-node Raft cluster that acknowledges a write too early.
//!
//! **The seeded defect, stated plainly, because a corpus program's bug is documented rather than
//! hidden:** the node tells its client a write is durable as soon as the entry is in *its own* log,
//! before a majority has it. That is a real mistake and a common one — the local append feels like
//! the durable moment and is not — and Raft's guarantee is about the majority, not about any one
//! log.
//!
//! Losing an acknowledged write therefore needs three things to fall together, which is what makes
//! this a depth-3 defect:
//!
//! 1. the leader appends an entry and acknowledges it,
//! 2. it dies before that entry reaches a majority,
//! 3. the term that follows elects a node without the entry, so it is never committed.
//!
//! None of the three is enough alone, and no ordering of a fault-free run produces them: without a
//! crash the leader keeps replicating and every acknowledged entry commits. That is asserted by
//! the suite rather than assumed — `tests/hunting` runs this with faults disabled and requires it
//! to stay clean, because a defect reachable without faults would measure the scheduler instead of
//! the search.
//!
//! **The check is the node's own.** On start it reads the acknowledgements it made before it died —
//! they are on its disk, `fsync`ed, so a crash keeps them — and before it exits it requires every
//! one of them to be present in what the cluster committed. Nothing about the defect is known to
//! the tool: the guest catches itself out and exits 1.

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read as _, Write as _},
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    time::Duration,
};

use prost::Message as _;
use raft::{prelude::*, storage::MemStorage, GetEntriesContext, StateRole};

/// Nodes in the cluster. Three is the smallest number with a majority to lose.
const NODES: u64 = 3;

/// Port every node listens on. Each has an address of its own, so one port is enough.
const PORT: u16 = 7000;

/// Virtual milliseconds between ticks.
const TICK_MS: u64 = 10;

/// Ticks a node runs for: long enough for an election, a crash, a recovery and another election.
const TICKS: u64 = 400;

/// Largest datagram the transport moves.
const MAX_DATAGRAM: usize = 8 * 1024;

/// Where a node writes what it has told its client is durable.
const ACKED: &str = "acked.log";

/// Where the node keeps its Raft log and vote across a crash.
///
/// Real Raft persists both, and a node that forgot them would be a different and much worse bug
/// than the seeded one: a restarted node claiming an empty log while its peers remember it voting
/// is amnesia, which Raft forbids, and the cluster misbehaves for reasons that have nothing to do
/// with acknowledging too early. Persisting them is what leaves exactly one defect in the program.
const RAFT_LOG: &str = "raft.log";
/// Where the vote and term are kept, for the same reason.
const RAFT_HARD: &str = "raft.hard";

/// Raft's own traffic, tagged apart from ours.
const TAG_RAFT: u8 = 0;

/// The address the world gives node `index`.
fn address_of(index: u64) -> SocketAddrV4 {
    let host = u32::try_from(index + 1).unwrap_or(1);
    SocketAddrV4::new(Ipv4Addr::from(0x0A42_0000 | host), PORT)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let index: u64 = std::env::args().nth(1).unwrap_or_else(|| "0".into()).parse()?;
    let id = index + 1;

    // What this node promised before it last died, each with the log index it was promised at.
    // Read first, because everything after depends on whether those promises are still good.
    let promised = read_acknowledged()?;

    let socket = UdpSocket::bind(address_of(index))?;
    socket.set_nonblocking(true)?;

    let config = Config { id, election_tick: 10, heartbeat_tick: 3, ..Default::default() };
    config.validate()?;

    let voters: Vec<u64> = (1..=NODES).collect();
    let storage = MemStorage::new_with_conf_state((voters, vec![]));
    // What survived the crash. Restored before the node runs, because a Raft node's identity is
    // its log and its vote, and one that starts without them is not the node that died.
    let restored = read_log()?;
    if !restored.is_empty() {
        storage.wl().append(&restored)?;
    }
    if let Some(hard) = read_hardstate()? {
        storage.wl().set_hardstate(hard);
    }
    let logger = slog::Logger::root(slog::Discard, slog::o!());
    let mut node = RawNode::new(&config, storage, &logger)?;

    let mut committed: HashSet<String> = HashSet::new();
    let mut leader_of_term: HashMap<u64, u64> = HashMap::new();
    let mut claimed_term = 0;
    let mut buf = vec![0u8; MAX_DATAGRAM];

    for _tick in 0..TICKS {
        while let Ok((len, _)) = socket.recv_from(&mut buf) {
            if len < 2 || buf[0] != TAG_RAFT {
                continue;
            }
            if let Ok(message) = Message::decode(&buf[1..len]) {
                node.step(message)?;
            }
        }

        node.tick();

        if node.raft.state == StateRole::Leader && node.raft.term > claimed_term {
            claimed_term = node.raft.term;
            leader_of_term.insert(claimed_term, id);

            // A client asks for a write, and this is where the defect lives. The entry goes into
            // this node's log and the acknowledgement is written and synced in the same breath —
            // before a single peer has seen it. Acknowledging here is the bug; acknowledging where
            // the entry appears in `committed` below would be correct.
            let payload = format!("t{claimed_term}");
            if node.propose(vec![], payload.clone().into_bytes()).is_ok() {
                // The index the entry will occupy, kept with the promise so a restarted node can
                // tell "this was overwritten" from "I have not caught up yet".
                acknowledge(node.raft.raft_log.last_index(), &payload)?;
            }
        }

        if !node.has_ready() {
            sleep_ms(TICK_MS);
            continue;
        }

        let mut ready = node.ready();
        send_all(&socket, ready.take_messages());
        for entry in ready.take_committed_entries() {
            remember(&mut committed, &entry);
        }
        if !ready.entries().is_empty() {
            node.mut_store().wl().append(ready.entries())?;
            persist_log(&node)?;
        }
        if let Some(hs) = ready.hs() {
            node.mut_store().wl().set_hardstate(hs.clone());
            persist_hardstate(hs)?;
        }
        send_all(&socket, ready.take_persisted_messages());

        let mut light = node.advance(ready);
        send_all(&socket, light.take_messages());
        for entry in light.take_committed_entries() {
            remember(&mut committed, &entry);
        }
        node.advance_apply();

        sleep_ms(TICK_MS);
    }

    // The invariant, checked against what this node promised before it died rather than against
    // what it happens to have now.
    //
    // The index is what makes this sound. A promise at index `i` is broken only once the cluster
    // has committed *past* `i` without it — that means something else occupies the slot and the
    // write is gone for good. While the node's commit index is still behind `i` it has simply not
    // caught up, and calling that a loss would be a test measuring its own impatience rather than
    // the guest's mistake.
    let reached = node.raft.raft_log.committed;
    let mut abstained = 0;
    for (index, write) in &promised {
        if committed.contains(write) {
            continue;
        }
        if *index > reached {
            abstained += 1;
            continue;
        }
        eprintln!(
            "node {id}: acknowledged write {write:?} at index {index} is gone; \
             the cluster committed past it, to {reached}"
        );
        return Err("an acknowledged write was lost".into());
    }

    println!(
        "node {id}: {} committed, {} promised, {abstained} not yet caught up",
        committed.len(),
        promised.len()
    );
    Ok(())
}

/// Records an entry the cluster committed.
fn remember(committed: &mut HashSet<String>, entry: &Entry) {
    if entry.data.is_empty() {
        return;
    }
    if let Ok(text) = std::str::from_utf8(&entry.data) {
        committed.insert(text.to_owned());
    }
}

/// Tells the client the write is durable, and makes that true of this node's own disk.
///
/// The `fsync` is not the defect. It is what makes the acknowledgement survive the crash, so the
/// restarted node can be held to it — a promise nobody remembers is a promise nobody can catch you
/// breaking.
fn acknowledge(index: u64, payload: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(ACKED)?;
    writeln!(file, "{index} {payload}")?;
    file.sync_all()
}

/// What this node acknowledged before it last died.
fn read_acknowledged() -> std::io::Result<Vec<(u64, String)>> {
    let mut text = String::new();
    match File::open(ACKED) {
        Ok(mut file) => {
            file.read_to_string(&mut text)?;
        }
        // A node that has never run has promised nothing, which is not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    }
    Ok(text
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(index, payload)| Some((index.parse().ok()?, payload.to_owned())))
        .collect())
}

/// Sends Raft's own traffic, tagged so the receiver can tell what it is.
fn send_all(socket: &UdpSocket, messages: Vec<Message>) {
    for message in messages {
        let to = message.to;
        let mut framed = vec![TAG_RAFT];
        framed.extend_from_slice(&message.encode_to_vec());
        // Raft identifiers are one-based; the world's node numbering is not.
        let _ = socket.send_to(&framed, address_of(to.saturating_sub(1)));
    }
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Writes the whole log out and syncs it.
///
/// Rewritten rather than appended to, because a new leader may overwrite entries this node had:
/// appending would keep the overwritten ones and restore a log the cluster has disowned.
fn persist_log(node: &RawNode<MemStorage>) -> Result<(), Box<dyn std::error::Error>> {
    let store = node.store();
    let first = store.first_index()?;
    let last = store.last_index()?;
    let mut bytes = Vec::new();
    if last >= first {
        for entry in store.entries(first, last + 1, None, GetEntriesContext::empty(false))? {
            let encoded = entry.encode_to_vec();
            bytes.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&encoded);
        }
    }
    write_synced(RAFT_LOG, &bytes)
}

/// Writes the vote and term out and syncs them.
fn persist_hardstate(hard: &HardState) -> Result<(), Box<dyn std::error::Error>> {
    write_synced(RAFT_HARD, &hard.encode_to_vec())
}

/// Replaces a file's contents and makes the replacement durable.
fn write_synced(path: &str, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// The log this node had when it died.
fn read_log() -> Result<Vec<Entry>, Box<dyn std::error::Error>> {
    let Some(bytes) = read_if_there(RAFT_LOG)? else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    let mut at = 0usize;
    // A crash can leave the last record half-written. Stopping at the first incomplete one keeps
    // the prefix that is whole, which is exactly what a durable log promises.
    while at + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into()?) as usize;
        at += 4;
        if at + len > bytes.len() {
            break;
        }
        if let Ok(entry) = Entry::decode(&bytes[at..at + len]) {
            entries.push(entry);
        }
        at += len;
    }
    Ok(entries)
}

/// The vote and term this node had when it died.
fn read_hardstate() -> Result<Option<HardState>, Box<dyn std::error::Error>> {
    match read_if_there(RAFT_HARD)? {
        Some(bytes) if !bytes.is_empty() => Ok(HardState::decode(&bytes[..]).ok()),
        _ => Ok(None),
    }
}

/// A file's contents, or nothing where a node has not run before.
fn read_if_there(path: &str) -> std::io::Result<Option<Vec<u8>>> {
    match File::open(path) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
