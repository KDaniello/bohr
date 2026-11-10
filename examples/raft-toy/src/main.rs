//! One node of a three-node Raft cluster that acknowledges a write too early.
//!
//! The seeded defect: a write is acknowledged once it is in the leader's own log, before a
//! majority has it.
//!
//! Losing an acknowledged write needs three things, making this a depth-3 defect:
//!
//! 1. the leader appends an entry and acknowledges it,
//! 2. it dies before that entry reaches a majority,
//! 3. the term that follows elects a node without the entry, so it is never committed.
//!
//! A fault-free run never produces them; `tests/hunting` checks that.
//!
//! The node checks itself: at start it reads its synced acknowledgements, and before exiting it
//! requires each to be committed, exiting 1 otherwise.

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read as _, Write as _},
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    time::Duration,
};

use prost::Message as _;
use raft::{prelude::*, storage::MemStorage, GetEntriesContext, StateRole};

/// Nodes in the cluster, the smallest with a majority to lose.
const NODES: u64 = 3;

/// Port every node listens on.
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
/// Persisted as Raft requires, so the seeded defect is the only one.
const RAFT_LOG: &str = "raft.log";
/// Where the vote and term are kept.
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

    // Promises made before the last death, with their log indices.
    let promised = read_acknowledged()?;

    let socket = UdpSocket::bind(address_of(index))?;
    socket.set_nonblocking(true)?;

    let config = Config { id, election_tick: 10, heartbeat_tick: 3, ..Default::default() };
    config.validate()?;

    let voters: Vec<u64> = (1..=NODES).collect();
    let storage = MemStorage::new_with_conf_state((voters, vec![]));
    // The persisted log and vote, restored before running.
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

            // The defect: acknowledged on local append. Correct would be on commit below.
            let payload = format!("t{claimed_term}");
            if node.propose(vec![], payload.clone().into_bytes()).is_ok() {
                // The index, to tell overwritten from not yet caught up.
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

    // A promise at index `i` is broken once the cluster commits past `i` without it.
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
/// The `fsync` keeps the acknowledgement across a crash, so the node can be held to it.
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
        // A first start has no promises.
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
        // Raft ids are one-based.
        let _ = socket.send_to(&framed, address_of(to.saturating_sub(1)));
    }
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Writes the whole log out and syncs it.
///
/// Rewritten whole, since a new leader may overwrite entries.
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
    // Stop at the first incomplete record.
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
