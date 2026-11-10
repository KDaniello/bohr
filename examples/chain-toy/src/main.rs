//! One node of a three-node chain that finalises a block on two votes of three.
//!
//! Height `h` has one proposer, node `h % 3`, which builds one block on its finalised tip. The hash
//! depends on parent, height and proposer, so a restarted proposer builds the same block. Each node
//! votes at most once per height, for the block on its own tip, and syncs the vote before sending.
//! A block with two votes is final; a node appends it, syncs, then reports it. Without a flag no two
//! nodes finalise different blocks at one height.
//!
//! Each flag adds one defect, and each needs the faults the scenario draws to show:
//!
//! - `--revert-finality`: the finalised log is written without a sync, and after a crash the node
//!   rebuilds the heights it lost from blocks of its own instead of fetching them from its peers;
//!   the hash it reports at a height it reported before changes.
//! - `--equivocate`: a proposer still waiting at its resend builds and signs a new block: two votes
//!   from one key at one height. It shows on many heights even without faults.
//! - `--fork-on-partition`: a node that hears no peer for a while finalises blocks of its own,
//!   alone; after the cut, its blocks and the cluster's differ at the same heights.
//! - `--stall-after-heal`: a node that was cut off stops proposing once it hears its peers again;
//!   the chain stops at the first height that node proposes.
//!
//! It answers JSON-RPC over HTTP on port 8545: `eth_getBlockByNumber` with `"finalized"`,
//! `"latest"` or a number, `eth_blockNumber`, and `chain_votes`, the signed votes it holds for its
//! recent heights as signer, height and hash.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream, UdpSocket},
    time::Duration,
};

/// Nodes in the cluster, the smallest with a disagreeable majority.
const NODES: u32 = 3;
/// Votes that make a block final.
const QUORUM: usize = 2;
/// Where each node takes the other nodes' messages.
const PORT: u16 = 7000;
/// Where each node answers JSON-RPC.
const RPC_PORT: u16 = 8545;
/// Virtual milliseconds between two turns of the loop.
const TICK_MS: u64 = 10;
/// Turns between a block finalising and the next being proposed: the chain's block time.
const SLOT: u64 = 5;
/// Turns between two resends of what a node has to say about the height it is at.
const RESEND: u64 = 3;
/// Turns a node hears no peer before it counts itself cut off.
const PATIENCE: u64 = 15;
/// Heights behind its tip a node still lists votes for.
const VOTE_WINDOW: u64 = 8;
/// Heights one catch-up message carries.
const CATCH_UP: u64 = 8;
/// Turns between two looks at the JSON-RPC callers.
const RPC_TURNS: u64 = 4;

const FINAL_LOG: &str = "final.log";
const VOTE_LOG: &str = "votes.log";
const EPOCH_LOG: &str = "epoch.log";

/// One flag per defect; none of them set is the correct program.
#[derive(Default)]
struct Flags {
    revert_finality: bool,
    equivocate: bool,
    fork_on_partition: bool,
    stall_after_heal: bool,
}

/// A block as the chain knows it: its hash and its parent's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Block {
    hash: u64,
    parent: u64,
}

struct Node {
    me: u32,
    flags: Flags,
    socket: UdpSocket,
    /// Finalised blocks, height 1 first.
    chain: Vec<Block>,
    final_log: File,
    vote_log: File,
    /// The block this node signed at each height.
    voted: BTreeMap<u64, Block>,
    /// Every proposal heard, by height: the proposer's blocks.
    proposals: BTreeMap<u64, Vec<Block>>,
    /// Who signed what: per height, per block, the signers.
    tally: BTreeMap<(u64, Block), BTreeSet<u32>>,
    /// Every signed vote heard or made, for `chain_votes`.
    signed: BTreeSet<(u64, u32, u64)>,
    /// The turn the tip last moved.
    tip_turn: u64,
    /// The turn a peer was last heard.
    heard_turn: u64,
    /// The highest tip a peer has reported.
    peer_tip: u64,
    /// Whether this node counted itself cut off, for `--stall-after-heal`.
    was_cut_off: bool,
    stalled: bool,
    /// How many times this node has started, for blocks that must differ across restarts.
    epoch: u64,
    /// Fresh blocks a proposer built at its current height, for `--equivocate`.
    rebuilt: u64,
    /// The turn this node last proposed.
    proposed_turn: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let me: u32 = args.next().unwrap_or_else(|| "0".into()).parse()?;
    let mut flags = Flags::default();
    for flag in args {
        match flag.as_str() {
            "--revert-finality" => flags.revert_finality = true,
            "--equivocate" => flags.equivocate = true,
            "--fork-on-partition" => flags.fork_on_partition = true,
            "--stall-after-heal" => flags.stall_after_heal = true,
            other => return Err(format!("no flag named {other}").into()),
        }
    }

    let epoch = bump_epoch()?;
    let socket = UdpSocket::bind(address_of(me, PORT))?;
    socket.set_nonblocking(true)?;
    let listener = TcpListener::bind(address_of(me, RPC_PORT))?;
    listener.set_nonblocking(true)?;

    let mut node = Node {
        me,
        socket,
        chain: Vec::new(),
        final_log: OpenOptions::new().create(true).append(true).open(FINAL_LOG)?,
        vote_log: OpenOptions::new().create(true).append(true).open(VOTE_LOG)?,
        voted: BTreeMap::new(),
        proposals: BTreeMap::new(),
        tally: BTreeMap::new(),
        signed: BTreeSet::new(),
        tip_turn: 0,
        heard_turn: 0,
        peer_tip: 0,
        was_cut_off: false,
        stalled: false,
        epoch,
        rebuilt: 0,
        proposed_turn: 0,
        flags,
    };
    node.restore()?;

    let mut clients: Vec<Client> = Vec::new();
    let mut turn: u64 = 0;
    loop {
        node.hear(turn)?;
        if turn % RPC_TURNS == 0 {
            accept(&listener, &mut clients);
            clients.retain_mut(|client| client.serve(&node));
        }
        node.act(turn)?;
        std::thread::sleep(Duration::from_millis(TICK_MS));
        turn += 1;
    }
}

/// The address the world gives node `index`.
fn address_of(index: u32, port: u16) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::from(0x0A42_0000 | (index + 1)), port)
}

/// FNV-1a, so a block's hash is the same on every node and every run.
fn hash_of(parent: u64, height: u64, proposer: u32, nonce: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in format!("{parent:016x}:{height}:{proposer}:{nonce}").bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn proposer_of(height: u64) -> u32 {
    (height % u64::from(NODES)) as u32
}

/// Counts this start, durably, and says which it is.
fn bump_epoch() -> std::io::Result<u64> {
    let before = std::fs::read_to_string(EPOCH_LOG).map(|s| s.lines().count()).unwrap_or(0);
    let mut log = OpenOptions::new().create(true).append(true).open(EPOCH_LOG)?;
    writeln!(log, "start")?;
    log.sync_all()?;
    Ok(before as u64 + 1)
}

/// Reads `height hash parent` lines.
fn read_blocks(path: &str) -> Vec<(u64, Block)> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let height = words.next()?.parse().ok()?;
            let hash = u64::from_str_radix(words.next()?, 16).ok()?;
            let parent = u64::from_str_radix(words.next()?, 16).ok()?;
            Some((height, Block { hash, parent }))
        })
        .collect()
}

impl Node {
    fn tip(&self) -> u64 {
        self.chain.len() as u64
    }

    fn tip_hash(&self) -> u64 {
        self.chain.last().map_or(0, |b| b.hash)
    }

    /// Takes back what survived the last crash: the finalised chain and every vote signed.
    fn restore(&mut self) -> std::io::Result<()> {
        for (height, block) in read_blocks(FINAL_LOG) {
            if height == self.tip() + 1 && block.parent == self.tip_hash() {
                self.chain.push(block);
            }
        }
        for (height, block) in read_blocks(VOTE_LOG) {
            self.voted.insert(height, block);
            self.signed.insert((height, self.me, block.hash));
        }
        Ok(())
    }

    /// Whether this node has heard no peer for long enough to count itself cut off.
    fn cut_off(&self, turn: u64) -> bool {
        turn.saturating_sub(self.heard_turn) >= PATIENCE
    }

    /// Whether this node, restarted under `--revert-finality`, rebuilds its lost heights itself.
    fn rebuilds(&self) -> bool {
        self.flags.revert_finality && self.epoch > 1
    }

    /// Whether it is still rebuilding what it lost.
    fn rebuilding(&self) -> bool {
        self.rebuilds() && self.tip() < self.peer_tip
    }

    fn send(&self, to: u32, message: &str) {
        let _ = self.socket.send_to(message.as_bytes(), address_of(to, PORT));
    }

    fn broadcast(&self, message: &str) {
        for peer in (0..NODES).filter(|p| *p != self.me) {
            self.send(peer, message);
        }
    }

    /// Takes every message that has arrived.
    fn hear(&mut self, turn: u64) -> std::io::Result<()> {
        let mut buf = [0u8; 2048];
        while let Ok((len, _)) = self.socket.recv_from(&mut buf) {
            let text = String::from_utf8_lossy(&buf[..len]).to_string();
            let words: Vec<&str> = text.split_whitespace().collect();
            let num = |i: usize| words.get(i).and_then(|w| w.parse::<u64>().ok());
            let hex = |i: usize| words.get(i).and_then(|w| u64::from_str_radix(w, 16).ok());
            match (words.first().copied(), num(1), hex(2), hex(3), num(4)) {
                (Some("P"), Some(height), Some(hash), Some(parent), Some(from)) => {
                    self.heard(turn, from);
                    if proposer_of(height) == from as u32 {
                        let block = Block { hash, parent };
                        let known = self.proposals.entry(height).or_default();
                        if !known.contains(&block) {
                            known.push(block);
                        }
                    }
                }
                (Some("V"), Some(height), Some(hash), Some(parent), Some(from)) => {
                    self.heard(turn, from);
                    let block = Block { hash, parent };
                    self.tally.entry((height, block)).or_default().insert(from as u32);
                    self.signed.insert((height, from as u32, hash));
                }
                (Some("T"), Some(height), _, _, Some(from)) => {
                    self.heard(turn, from);
                    self.peer_tip = self.peer_tip.max(height);
                    self.catch_up(from as u32, height);
                }
                (Some("C"), Some(height), Some(hash), Some(parent), Some(from)) => {
                    self.heard(turn, from);
                    if !self.rebuilds()
                        && height == self.tip() + 1
                        && parent == self.tip_hash()
                    {
                        self.finalise(Block { hash, parent }, turn)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Notes that a peer was heard; under `--stall-after-heal`, hearing peers after a cut stops
    /// proposing.
    fn heard(&mut self, turn: u64, from: u64) {
        if from as u32 == self.me {
            return;
        }
        if self.cut_off(turn) && turn > PATIENCE {
            self.was_cut_off = true;
        }
        if self.flags.stall_after_heal && self.was_cut_off {
            self.stalled = true;
        }
        self.heard_turn = turn;
    }

    /// Sends a peer that is behind the blocks it is missing, a few at a time.
    fn catch_up(&self, peer: u32, peer_tip: u64) {
        let last = self.tip().min(peer_tip + CATCH_UP);
        for height in peer_tip + 1..=last {
            let block = self.chain[(height - 1) as usize];
            self.send(
                peer,
                &format!("C {height} {:x} {:x} {}", block.hash, block.parent, self.me),
            );
        }
    }

    /// Appends a final block and reports it only once it is durable, unless the flag says not to
    /// sync.
    fn finalise(&mut self, block: Block, turn: u64) -> std::io::Result<()> {
        let height = self.tip() + 1;
        writeln!(self.final_log, "{height} {:x} {:x}", block.hash, block.parent)?;
        if !self.flags.revert_finality {
            self.final_log.sync_all()?;
        }
        self.chain.push(block);
        self.tip_turn = turn;
        self.rebuilt = 0;
        Ok(())
    }

    /// Signs a block at a height, durably, then tells everybody.
    fn vote(&mut self, height: u64, block: Block) -> std::io::Result<()> {
        writeln!(self.vote_log, "{height} {:x} {:x}", block.hash, block.parent)?;
        self.vote_log.sync_all()?;
        self.voted.insert(height, block);
        self.signed.insert((height, self.me, block.hash));
        self.tally.entry((height, block)).or_default().insert(self.me);
        self.broadcast(&format!("V {height} {:x} {:x} {}", block.hash, block.parent, self.me));
        Ok(())
    }

    /// One turn of the protocol.
    fn act(&mut self, turn: u64) -> std::io::Result<()> {
        let next = self.tip() + 1;
        let paced = turn >= self.tip_turn + SLOT;

        // A node building alone: a fork while cut off, or a rebuild after a crash.
        let alone = (self.flags.fork_on_partition && self.cut_off(turn)) || self.rebuilding();
        if alone {
            if paced {
                let parent = self.tip_hash();
                let block = Block { hash: hash_of(parent, next, self.me, self.epoch * 1000 + 1), parent };
                self.signed.insert((next, self.me, block.hash));
                self.finalise(block, turn)?;
            }
            return Ok(());
        }

        if proposer_of(next) == self.me && paced && !self.stalled && !self.voted.contains_key(&next)
        {
            let parent = self.tip_hash();
            let block = Block { hash: hash_of(parent, next, self.me, 0), parent };
            self.proposals.entry(next).or_default().push(block);
            self.vote(next, block)?;
            self.broadcast(&format!("P {next} {:x} {:x} {}", block.hash, block.parent, self.me));
            self.proposed_turn = turn;
        }

        if !self.voted.contains_key(&next) {
            let tip_hash = self.tip_hash();
            let fitting = self
                .proposals
                .get(&next)
                .and_then(|known| known.iter().find(|b| b.parent == tip_hash).copied());
            if let Some(block) = fitting {
                self.vote(next, block)?;
            }
        }

        loop {
            let next = self.tip() + 1;
            let tip_hash = self.tip_hash();
            let ready = self
                .tally
                .range((next, Block { hash: 0, parent: 0 })..)
                .take_while(|((h, _), _)| *h == next)
                .find(|((_, b), signers)| b.parent == tip_hash && signers.len() >= QUORUM)
                .map(|((_, b), _)| *b);
            match ready {
                Some(block) => self.finalise(block, turn)?,
                None => break,
            }
        }

        if turn % RESEND == 0 {
            self.resend(turn)?;
        }
        // What is far behind the tip is never needed again.
        let floor = self.tip().saturating_sub(VOTE_WINDOW);
        self.proposals = self.proposals.split_off(&floor);
        self.tally = self.tally.split_off(&(floor, Block { hash: 0, parent: 0 }));
        self.signed = self.signed.split_off(&(floor, 0, 0));
        Ok(())
    }

    /// Says again what this node has to say about the height it is at.
    fn resend(&mut self, turn: u64) -> std::io::Result<()> {
        let next = self.tip() + 1;
        if let Some(block) = self.voted.get(&next).copied() {
            if proposer_of(next) == self.me && !self.stalled {
                if self.flags.equivocate && turn > self.proposed_turn {
                    // The defect: a proposer still waiting builds a new block and signs it too.
                    self.rebuilt += 1;
                    let parent = self.tip_hash();
                    let fresh = Block { hash: hash_of(parent, next, self.me, self.rebuilt), parent };
                    self.proposals.entry(next).or_default().push(fresh);
                    self.signed.insert((next, self.me, fresh.hash));
                    self.tally.entry((next, fresh)).or_default().insert(self.me);
                    self.broadcast(&format!("V {next} {:x} {:x} {}", fresh.hash, fresh.parent, self.me));
                    self.broadcast(&format!("P {next} {:x} {:x} {}", fresh.hash, fresh.parent, self.me));
                } else {
                    self.broadcast(&format!("P {next} {:x} {:x} {}", block.hash, block.parent, self.me));
                }
            }
            self.broadcast(&format!("V {next} {:x} {:x} {}", block.hash, block.parent, self.me));
        }
        self.broadcast(&format!("T {} 0 0 {}", self.tip(), self.me));
        Ok(())
    }

    /// The answer to one JSON-RPC call, as the `result` or `error` member's text.
    fn rpc(&self, method: &str, first_param: Option<&str>) -> Result<String, String> {
        let block = |height: u64| -> String {
            match height.checked_sub(1).and_then(|i| self.chain.get(i as usize)) {
                Some(b) => format!(
                    "{{\"number\":\"0x{height:x}\",\"hash\":\"0x{:016x}\",\"parentHash\":\"0x{:016x}\"}}",
                    b.hash, b.parent
                ),
                None => "null".to_owned(),
            }
        };
        match method {
            "eth_blockNumber" => Ok(format!("\"0x{:x}\"", self.tip())),
            "eth_getBlockByNumber" => match first_param {
                Some("finalized" | "latest" | "safe") => Ok(block(self.tip())),
                Some(number) => match u64::from_str_radix(number.trim_start_matches("0x"), 16) {
                    Ok(height) => Ok(block(height)),
                    Err(_) => Err("bad block number".to_owned()),
                },
                None => Err("missing block number".to_owned()),
            },
            "chain_votes" => {
                let floor = self.tip().saturating_sub(VOTE_WINDOW);
                let votes: Vec<String> = self
                    .signed
                    .range((floor, 0, 0)..)
                    .map(|(height, signer, hash)| {
                        format!(
                            "{{\"signer\":\"{signer}\",\"height\":\"0x{height:x}\",\"hash\":\"0x{hash:016x}\"}}"
                        )
                    })
                    .collect();
                Ok(format!("[{}]", votes.join(",")))
            }
            _ => Err("method not found".to_owned()),
        }
    }
}

/// One JSON-RPC caller and what it has sent so far.
struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
}

fn accept(listener: &TcpListener, clients: &mut Vec<Client>) {
    while let Ok((stream, _)) = listener.accept() {
        if stream.set_nonblocking(true).is_ok() {
            clients.push(Client { stream, buf: Vec::new() });
        }
    }
}

impl Client {
    /// Answers every complete request that has arrived; false once the caller is gone.
    ///
    /// One read per look; the rest waits.
    fn serve(&mut self, node: &Node) -> bool {
        let mut chunk = [0u8; 16 * 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => return false,
            Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
        loop {
            let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                return true;
            };
            let head = String::from_utf8_lossy(&self.buf[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|n| n.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if self.buf.len() < end + 4 + length {
                return true;
            }
            let body = String::from_utf8_lossy(&self.buf[end + 4..end + 4 + length]).to_string();
            self.buf.drain(..end + 4 + length);
            let answer = |call: &str| {
                let method = string_after(call, "\"method\"").unwrap_or_default();
                let first = call.find("\"params\"").and_then(|at| string_after(&call[at..], "["));
                let id = call
                    .find("\"id\":")
                    .map(|at| &call[at + 5..])
                    .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>())
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| "null".to_owned());
                let member = match node.rpc(&method, first.as_deref()) {
                    Ok(result) => format!("\"result\":{result}"),
                    Err(message) => {
                        format!("\"error\":{{\"code\":-32601,\"message\":\"{message}\"}}")
                    }
                };
                format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},{member}}}")
            };
            // A batch is answered as one, call by call, in the order asked.
            let reply = if body.trim_start().starts_with('[') {
                let calls: Vec<String> = objects_in(&body).iter().map(|c| answer(c)).collect();
                format!("[{}]", calls.join(","))
            } else {
                answer(&body)
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                reply.len()
            );
            if self.stream.write_all(response.as_bytes()).is_err() {
                return false;
            }
        }
    }
}

/// The top-level objects of a JSON array, as text, skipping braces inside strings.
fn objects_in(text: &str) -> Vec<&str> {
    let (mut depth, mut start, mut in_string, mut escaped) = (0usize, 0usize, false, false);
    let mut out = Vec::new();
    for (i, c) in text.char_indices() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    out
}

/// The first string literal after `marker` in `text`.
fn string_after(text: &str, marker: &str) -> Option<String> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let open = rest.find('"')?;
    let between = &rest[..open];
    if between.chars().any(|c| !matches!(c, ':' | ' ' | '[')) {
        return None;
    }
    let rest = &rest[open + 1..];
    Some(rest[..rest.find('"')?].to_owned())
}
