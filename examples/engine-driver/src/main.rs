//! The other half of an OP Stack node: the thing that drives the execution client.
//!
//! An execution client idles until driven. This caller authenticates like a consensus client and
//! drives it like one in sequencer mode.
//!
//! It never builds a block: it asks the client to build, hands the payload text back for
//! validation, and names the returned hash as head. It parses no payload.
//!
//! It records every hash the client called `VALID`, and leaves the head's state root in a file for
//! a projection.
//!
//! The scenario runs the client and this driver as separate nodes.
//!
//! `engine-driver chain` drives an Ethereum execution client instead, as a beacon node would: a
//! block every interval, built from the client's own pool of transactions, and, with `--follower`,
//! every block handed to a second client, which must reach the same state.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddrV4, TcpStream},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Where the shared secret is, on every node's filesystem.
///
/// A file in the prepared state, read by both nodes.
const SECRET_PATH: &str = "/jwt.hex";

/// The authenticated endpoint, which is the one a consensus client uses.
const ENGINE_PORT: u16 = 8551;
/// The open one, which needs no token.
const PUBLIC_PORT: u16 = 8545;

/// How long to keep trying before giving up on the node ever listening.
const ATTEMPTS: u32 = 400;
/// How long to wait between attempts, in virtual time.
const RETRY: Duration = Duration::from_millis(5);

/// Rounds of [`ATTEMPTS`] a client is given to start, about two seconds each: reth opens its
/// engine API ten seconds in.
const STARTUP_ROUNDS: u32 = 15;

/// Largest answer read back: a payload of a full block of transfers, hundreds of kilobytes, fits.
const MAX_ANSWER: usize = 4 * 1024 * 1024;

/// How many blocks the client is asked to build.
const BLOCKS: u32 = 4;

/// How many the branch after a rewind is.
const BRANCH: u32 = 2;

/// Seconds between blocks, as on an OP Stack chain.
const BLOCK_TIME: u64 = 2;

/// Where the head's state root is left, for a projection to compare implementations by.
///
/// At the root, so no directory has to be made.
const STATE_ROOT_PATH: &str = "/state-root";

/// Thirty-two zero bytes, as no beacon chain is behind this caller.
const ZERO32: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
/// Twenty, for an address.
const ZERO20: &str = "0x0000000000000000000000000000000000000000";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("chain") {
        std::process::exit(chain_mode(&args[2..]));
    }
    std::process::exit(drive());
}

/// The address the world gives node `index`.
fn at(index: u8, port: u16) -> SocketAddrV4 {
    SocketAddrV4::new([10, 66, 0, index + 1].into(), port)
}

/// What `chain` was told: the client that builds, the one that follows, how many blocks, and how
/// far apart.
struct ChainArgs {
    leader: u8,
    follower: Option<u8>,
    blocks: u32,
    every: Duration,
}

fn chain_args(args: &[String]) -> Result<ChainArgs, String> {
    let mut out =
        ChainArgs { leader: 0, follower: None, blocks: 20, every: Duration::from_secs(2) };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--leader" => out.leader = value()?.parse().map_err(|e| format!("--leader: {e}"))?,
            "--follower" => {
                out.follower = Some(value()?.parse().map_err(|e| format!("--follower: {e}"))?);
            }
            "--blocks" => out.blocks = value()?.parse().map_err(|e| format!("--blocks: {e}"))?,
            "--every-ms" => {
                out.every = Duration::from_millis(
                    value()?.parse().map_err(|e| format!("--every-ms: {e}"))?,
                );
            }
            other => return Err(format!("no argument {other:?}")),
        }
    }
    Ok(out)
}

/// A beacon node's loop over one or two execution clients: build on the leader from its pool,
/// validate on it, hand the same payload to the follower, and make it head on both.
///
/// Every block is final once the next one is built, so a crash has something to contradict. Each
/// line it prints names the block, the leader's state root, and what the follower answered.
fn chain_mode(args: &[String]) -> i32 {
    let args = match chain_args(args) {
        Ok(args) => args,
        Err(why) => {
            eprintln!("engine-driver chain: {why}");
            return 2;
        }
    };
    let secret = match read_secret() {
        Ok(secret) => secret,
        Err(e) => {
            eprintln!("cannot read {SECRET_PATH}: {e}");
            return 2;
        }
    };
    // Every client is waited for before the first block: one that misses a block has no peer to
    // fetch it from, and follows nothing after it.
    let mut genesis_of = Vec::new();
    for client in std::iter::once(args.leader).chain(args.follower) {
        let Some(genesis) = (0..STARTUP_ROUNDS).find_map(|_| {
            call_at(client, PUBLIC_PORT, None, "eth_getBlockByNumber", r#"["0x0",false]"#)
        }) else {
            return nothing_asked(&format!("node {client} never answered on its public endpoint"));
        };
        let Some(hash) = field(&genesis, "hash") else {
            return nothing_asked(&format!("no genesis block: {}", body(&genesis, 300)));
        };
        println!("node {client} genesis {hash}");
        genesis_of.push((hash, field(&genesis, "timestamp").unwrap_or_default()));
    }
    if genesis_of.iter().any(|(hash, _)| *hash != genesis_of[0].0) {
        println!("the clients start from different genesis blocks");
    }
    let mut built = 0;
    for number in 1..=args.blocks {
        // Built on the leader's own head, asked afresh each block, as a beacon node follows its
        // client: a driver restarted, or a client come back behind, carries on from where the
        // client is.
        let Some(latest) =
            call_at(args.leader, PUBLIC_PORT, None, "eth_getBlockByNumber", r#"["latest",false]"#)
        else {
            println!("block {number} not built: the leader does not name its head");
            std::thread::sleep(args.every);
            continue;
        };
        let (Some(head), Some(stamp)) = (field(&latest, "hash"), field(&latest, "timestamp"))
        else {
            println!("block {number} not built: {}", body(&latest, 300));
            std::thread::sleep(args.every);
            continue;
        };
        let parent = field(&latest, "parentHash").filter(|p| p != ZERO32).unwrap_or(head.clone());
        let finalized = parent;
        let stamp = hex_u64(&stamp) + args.every.as_secs().max(1);
        // A fresh token each block: one is valid for a minute of the node's clock.
        let token = bearer(&secret);
        let attributes = format!(
            r#"{{"timestamp":"{}","prevRandao":"{ZERO32}","suggestedFeeRecipient":"{ZERO20}"}}"#,
            hex(stamp)
        );
        let state = forkchoice_state(&head, &finalized);
        let Some(started) = call_at(
            args.leader,
            ENGINE_PORT,
            Some(&token),
            "engine_forkchoiceUpdatedV1",
            &format!("[{state},{attributes}]"),
        ) else {
            println!("block {number} not built: the leader stopped answering");
            std::thread::sleep(args.every);
            continue;
        };
        let Some(id) = field(&started, "payloadId").filter(|id| id != "null") else {
            println!("block {number} not built: {}", body(&started, 300));
            std::thread::sleep(args.every);
            continue;
        };
        // The slot passes before the payload is asked for, as a beacon node waits: a client
        // starts from an empty block and fills it from its pool while the slot runs.
        std::thread::sleep(args.every);
        let Some(got) = call_at(
            args.leader,
            ENGINE_PORT,
            Some(&token),
            "engine_getPayloadV1",
            &format!(r#"["{id}"]"#),
        ) else {
            println!("block {number} not built: the leader stopped answering at getPayload");
            continue;
        };
        let text = body(&got, MAX_ANSWER);
        let Some(payload) = object(&text, "result") else {
            println!("block {number} not built: {}", body(&got, 300));
            continue;
        };
        let Some(hash) = field(&payload, "blockHash") else { continue };
        let root = field(&payload, "stateRoot").unwrap_or_default();
        let mut answers = Vec::new();
        for client in std::iter::once(args.leader).chain(args.follower) {
            let valid = call_at(
                client,
                ENGINE_PORT,
                Some(&token),
                "engine_newPayloadV1",
                &format!("[{payload}]"),
            )
            .map_or_else(|| "unanswered".to_owned(), |a| status(&a));
            let next = forkchoice_state(&hash, &head);
            let chosen = call_at(
                client,
                ENGINE_PORT,
                Some(&token),
                "engine_forkchoiceUpdatedV1",
                &format!("[{next},null]"),
            )
            .map_or_else(|| "unanswered".to_owned(), |a| status(&a));
            answers.push(format!("node {client} {valid}/{chosen}"));
        }
        println!("block {number} {hash} root {root} {}", answers.join(" "));
        built += 1;
    }
    println!("built {built} of {}", args.blocks);
    0
}

/// A forkchoice state naming `head`, with `finalized` as both safe and final.
fn forkchoice_state(head: &str, finalized: &str) -> String {
    format!(
        r#"{{"headBlockHash":"{head}","safeBlockHash":"{finalized}","finalizedBlockHash":"{finalized}"}}"#
    )
}

/// The status an engine answer gives, or the start of the answer where it gives none.
fn status(answer: &str) -> String {
    field(answer, "status").unwrap_or_else(|| body(answer, 120))
}

/// A run in which the client was never reached, and so was never held to anything.
///
/// Not a finding: a node killed before answering promised nothing. The counts read zero.
fn nothing_asked(why: &str) -> i32 {
    println!("nothing asked: {why}");
    println!("acknowledged 0 verified 0 forked 0 behind 0 unanswered 0");
    0
}

/// Asks the execution client what it is, what it can do, and where its chain begins.
///
/// Methods, genesis block and its timestamp are asked, since they differ per chain.
fn drive() -> i32 {
    let secret = match read_secret() {
        Ok(secret) => secret,
        Err(e) => {
            eprintln!("cannot read {SECRET_PATH}: {e}");
            return 2;
        }
    };

    // The open endpoint first, so a later failure points at the token.
    let Some(chain) = call(PUBLIC_PORT, None, "eth_chainId", "[]") else {
        return nothing_asked("the node never answered on its public endpoint");
    };
    println!("chainId {}", field(&chain, "result").unwrap_or_else(|| "?".to_owned()));

    let token = bearer(&secret);
    let Some(capabilities) = call(
        ENGINE_PORT,
        Some(&token),
        "engine_exchangeCapabilities",
        r#"[["engine_forkchoiceUpdatedV3","engine_getPayloadV3","engine_newPayloadV3"]]"#,
    ) else {
        return nothing_asked("the node never answered on its engine endpoint");
    };
    if capabilities.contains("\"error\"") {
        return nothing_asked(&format!("the engine refused: {}", body(&capabilities, 300)));
    }
    println!("capabilities {}", body(&capabilities, 2000));

    // Genesis, the only head nameable before building; its timestamp picks the fork.
    let Some(genesis) = call(PUBLIC_PORT, None, "eth_getBlockByNumber", r#"["0x0",false]"#) else {
        return nothing_asked("the node stopped answering before it named its genesis");
    };
    let Some(head) = field(&genesis, "hash") else {
        return nothing_asked(&format!("no genesis block: {}", body(&genesis, 300)));
    };
    let timestamp = field(&genesis, "timestamp").unwrap_or_default();
    let gas_limit = field(&genesis, "gasLimit").unwrap_or_default();
    println!("genesis {head} timestamp {timestamp} gasLimit {gas_limit}");

    // A forkchoice to the block the client already has.
    let Some(answer) = forkchoice(&token, &head, &head, &head, None) else {
        return nothing_asked("the node stopped answering at the first forkchoice");
    };
    if !answer.contains("\"VALID\"") {
        return nothing_asked(&format!(
            "the client refused its own genesis: {}",
            body(&answer, 300)
        ));
    }

    let mut chain = Chain {
        head,
        finalized: String::new(),
        promised: Vec::new(),
        state_root: String::new(),
        gas_limit: gas_limit.clone(),
        stamp: hex_u64(&timestamp),
    };

    // A block that cannot be built is not a finding; the node may be dead.
    for number in 1..=BLOCKS {
        chain.stamp += BLOCK_TIME;
        match build_one(&token, &mut chain, number) {
            Ok(()) => println!("block {number} {} root {}", chain.head, chain.state_root),
            Err(why) => {
                println!("block {number} not built: {why}");
                break;
            }
        }
    }

    // Rewind to the newest final block and build a different branch; nothing final is skipped.
    if std::env::args().any(|arg| arg == "reorg") {
        match reorg(&token, &mut chain) {
            Ok((target, built)) => println!("reorg to {target} built {built}"),
            Err(why) => println!("reorg not made: {why}"),
        }
    }

    let verdict = verify(&chain);
    println!(
        "acknowledged {} verified {} forked {} behind {} unanswered {}",
        chain.promised.len(),
        verdict.verified,
        verdict.forked.len(),
        verdict.behind,
        verdict.unanswered
    );
    for (number, promised, found) in &verdict.forked {
        println!("FINDING: block {number} was final as {promised} and came back as {found}");
    }

    // Written once at the end, for the projection.
    if let Err(e) = std::fs::write(STATE_ROOT_PATH, chain.state_root.as_bytes()) {
        eprintln!("cannot write {STATE_ROOT_PATH}: {e}");
        return 2;
    }
    println!("head {} finalized {}", chain.head, chain.finalized);
    i32::from(!verdict.forked.is_empty())
}

/// What the driver knows about the chain, which is only ever what the client told it.
struct Chain {
    /// The newest block the client called `VALID`.
    head: String,
    /// The newest block the client was told is final.
    finalized: String,
    /// Every block the client was ever told is final, which is the promise a crash must not undo.
    promised: Vec<(u32, String)>,
    /// The head's state root, which implementations must agree on.
    state_root: String,
    /// The gas limit, carried from genesis, which an OP client requires.
    gas_limit: String,
    /// What the head was stamped with, which the next block has to be built past.
    stamp: u64,
}

/// What became of the promises, once the client has been asked about every one of them.
#[derive(Default)]
struct Verdict {
    /// Heights where the final block is still there.
    verified: u32,
    /// Heights with a different block; each is a finding.
    forked: Vec<(u32, String, String)>,
    /// Heights it has no block at, because it came back behind what it had answered for.
    ///
    /// Counted only: a client may lose unpersisted blocks to a crash.
    behind: u32,
    /// Questions the client never answered, which are not failures.
    unanswered: u32,
}

/// Holds the client to what it said, after whatever the run did to it.
///
/// Asked by height, which tells falling behind (allowed) from rewriting a final block (a fault).
///
/// Every hash compared came from the client.
fn verify(chain: &Chain) -> Verdict {
    let mut verdict = Verdict::default();
    for (number, hash) in &chain.promised {
        let params = format!(r#"["{}",false]"#, hex(u64::from(*number)));
        let Some(answer) = call(PUBLIC_PORT, None, "eth_getBlockByNumber", &params) else {
            verdict.unanswered += 1;
            continue;
        };
        match field(&answer, "hash") {
            Some(found) if &found == hash => verdict.verified += 1,
            Some(found) => verdict.forked.push((*number, hash.clone(), found)),
            None if body(&answer, MAX_ANSWER).replace(' ', "").contains("\"result\":null") => {
                verdict.behind += 1;
            }
            // An unreadable answer is skipped.
            None => verdict.unanswered += 1,
        }
    }
    verdict
}

/// Rewinds to the newest final block and builds a different chain from it.
///
/// The sibling differs only by a timestamp one second later.
fn reorg(token: &str, chain: &mut Chain) -> Result<(String, u32), String> {
    let Some((number, target)) = chain.promised.last().cloned() else {
        return Err("nothing was ever final, so there is nothing to rewind to".to_owned());
    };
    chain.head = target.clone();
    chain.stamp = chain.stamp.saturating_sub(BLOCK_TIME) + 1;

    let mut built = 0;
    for step in 1..=BRANCH {
        chain.stamp += BLOCK_TIME;
        build_one(token, chain, number + step)?;
        built += 1;
    }
    Ok((target, built))
}

/// One turn of the loop an OP consensus client runs in sequencer mode.
///
/// Build, fetch, validate, and set as head. The payload stays text except for three fields the
/// client produced.
fn build_one(token: &str, chain: &mut Chain, number: u32) -> Result<(), String> {
    // `noTxPool`, zero fee recipient and zero randomness keep blocks a function of the tape.
    let attributes = format!(
        r#"{{"timestamp":"{}","prevRandao":"{ZERO32}","suggestedFeeRecipient":"{ZERO20}","noTxPool":true,"transactions":[],"gasLimit":"{}"}}"#,
        hex(chain.stamp),
        chain.gas_limit
    );
    let started = forkchoice(token, &chain.head, &chain.head, &chain.finalized, Some(&attributes))
        .ok_or_else(|| "the node stopped answering at forkchoice".to_owned())?;
    let id = field(&started, "payloadId")
        .filter(|id| id != "null")
        .ok_or_else(|| format!("no payload was started: {}", body(&started, 300)))?;

    let got = call(ENGINE_PORT, Some(token), "engine_getPayloadV1", &format!(r#"["{id}"]"#))
        .ok_or_else(|| "the node stopped answering at getPayload".to_owned())?;
    // Later versions wrap the payload; it is unwrapped and passed back untouched.
    let body_text = body(&got, MAX_ANSWER);
    let payload = object(&body_text, "executionPayload")
        .or_else(|| object(&body_text, "result"))
        .ok_or_else(|| format!("no payload came back: {}", body(&got, 300)))?;

    let hash = field(&payload, "blockHash").ok_or_else(|| {
        format!("the payload has no hash: {}", &payload[..payload.len().min(300)])
    })?;
    let root = field(&payload, "stateRoot").unwrap_or_default();

    let validated = call(ENGINE_PORT, Some(token), "engine_newPayloadV1", &format!("[{payload}]"))
        .ok_or_else(|| "the node stopped answering at newPayload".to_owned())?;
    if !validated.contains("\"VALID\"") {
        return Err(format!("the client refused what it built: {}", body(&validated, 300)));
    }

    // Finality lags the head by one block, so a crash has something to contradict.
    let previous = std::mem::replace(&mut chain.head, hash);
    let accepted = forkchoice(token, &chain.head, &previous, &previous, None)
        .ok_or_else(|| "the node stopped answering at the final forkchoice".to_owned())?;
    if !accepted.contains("\"VALID\"") {
        return Err(format!("the client refused its own new head: {}", body(&accepted, 300)));
    }
    // Recorded once the client accepts the forkchoice, and only once.
    chain.finalized = previous.clone();
    if chain.promised.last().map(|(_, held)| held) != Some(&previous) {
        chain.promised.push((number.saturating_sub(1), previous));
    }
    chain.state_root = root;
    Ok(())
}

/// One `engine_forkchoiceUpdated`, with attributes where the caller is asking for a block.
///
/// V1 throughout, since this chain stays before the withdrawals fork.
fn forkchoice(
    token: &str,
    head: &str,
    safe: &str,
    finalized: &str,
    attributes: Option<&str>,
) -> Option<String> {
    // A missing hash is sent as zero.
    let or_zero = |hash: &str| if hash.is_empty() { ZERO32.to_owned() } else { hash.to_owned() };
    let state = format!(
        r#"{{"headBlockHash":"{}","safeBlockHash":"{}","finalizedBlockHash":"{}"}}"#,
        or_zero(head),
        or_zero(safe),
        or_zero(finalized)
    );
    let params = format!("[{state},{}]", attributes.unwrap_or("null"));
    call(ENGINE_PORT, Some(token), "engine_forkchoiceUpdatedV1", &params)
}

/// The shared secret, as the thirty-two bytes it is written as hexadecimal.
fn read_secret() -> std::io::Result<Vec<u8>> {
    for _ in 0..ATTEMPTS {
        match std::fs::read_to_string(SECRET_PATH) {
            Ok(text) => {
                let text = text.trim().trim_start_matches("0x");
                let bytes: Option<Vec<u8>> = (0..text.len() / 2)
                    .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
                    .collect();
                if let Some(bytes) = bytes.filter(|b| b.len() == 32) {
                    return Ok(bytes);
                }
            }
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            Err(_) => {}
        }
        std::thread::sleep(RETRY);
    }
    Err(std::io::Error::new(ErrorKind::NotFound, "no secret appeared"))
}

/// A bearer token the node's own library will accept: signed with the shared secret, and stamped
/// with a time taken from the same clock the node reads.
///
/// The guest's clock is the world's, so the token matches what the node reads.
#[allow(clippy::disallowed_methods, reason = "a guest's clock is the world's, and is intercepted")]
fn bearer(secret: &[u8]) -> String {
    let issued = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let header = base64url(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = base64url(format!(r#"{{"iat":{issued}}}"#).as_bytes());
    let signed = format!("{header}.{claims}");

    format!("{signed}.{}", base64url(&hmac_sha256(secret, signed.as_bytes())))
}

/// HMAC-SHA256 (RFC 2104), what a JWT's `HS256` names.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.iter().map(|b| b ^ byte).collect::<Vec<u8>>();
    let mut inner = pad(0x36);
    inner.extend_from_slice(message);
    let mut outer = pad(0x5c);
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

/// SHA-256 (FIPS 180-4).
fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&((message.len() as u64) * 8).to_be_bytes());
    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(choose).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            (hh, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (state, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *state = state.wrapping_add(add);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Sends one request to node zero and reads the answer, retrying while nobody is listening yet.
fn call(port: u16, token: Option<&str>, method: &str, params: &str) -> Option<String> {
    call_at(0, port, token, method, params)
}

/// Sends one request to node `index` and reads the answer, retrying until the node listens.
fn call_at(
    index: u8,
    port: u16,
    token: Option<&str>,
    method: &str,
    params: &str,
) -> Option<String> {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#);
    let authorization =
        token.map_or_else(String::new, |token| format!("Authorization: Bearer {token}\r\n"));
    let request = format!(
        "POST / HTTP/1.1\r\nHost: 10.66.0.1\r\nContent-Type: application/json\r\n\
         {authorization}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    for _ in 0..ATTEMPTS {
        let Ok(mut stream) = TcpStream::connect(at(index, port)) else {
            std::thread::sleep(RETRY);
            continue;
        };
        if stream.write_all(request.as_bytes()).is_err() {
            std::thread::sleep(RETRY);
            continue;
        }
        // `Connection: close`, so the answer ends with the stream.
        let mut answer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    answer.extend_from_slice(&chunk[..read]);
                    if answer.len() >= MAX_ANSWER {
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if !answer.is_empty() {
            return Some(String::from_utf8_lossy(&answer).into_owned());
        }
        std::thread::sleep(RETRY);
    }
    None
}

/// One string field of a JSON object, for answers of known shape.
fn field(answer: &str, name: &str) -> Option<String> {
    let at = answer.find(&format!("\"{name}\""))? + name.len() + 3;
    let rest = answer.get(at..)?.trim_start_matches([':', ' ', '"']);
    let end = rest.find(['"', ',', '}'])?;
    Some(rest[..end].to_owned())
}

/// The object a named field holds, as the text between its braces.
///
/// Brace matching, skipping strings, so the payload passes back unparsed.
fn object(text: &str, name: &str) -> Option<String> {
    let at = text.find(&format!("\"{name}\""))?;
    let rest = text.get(at + name.len() + 2..)?;
    let open = rest.find('{')?;
    let bytes = rest.as_bytes();
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for (i, byte) in bytes.iter().enumerate().skip(open) {
        match *byte {
            _ if escaped => escaped = false,
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            _ if quoted => {}
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return rest.get(open..=i).map(str::to_owned);
                }
            }
            _ => {}
        }
    }
    None
}

/// A quantity as the engine spells it, and back.
fn hex_u64(text: &str) -> u64 {
    u64::from_str_radix(text.trim_start_matches("0x"), 16).unwrap_or_default()
}

fn hex(value: u64) -> String {
    format!("0x{value:x}")
}

/// The body of an answer, which is what follows the blank line after its headers.
///
/// Bounded by the caller.
fn body(answer: &str, take: usize) -> String {
    answer.split("\r\n\r\n").nth(1).unwrap_or(answer).trim().chars().take(take).collect()
}

/// Base64 as a token spells it: the URL alphabet, and no padding.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for group in bytes.chunks(3) {
        let mut packed = 0u32;
        for (i, byte) in group.iter().enumerate() {
            packed |= u32::from(*byte) << (16 - 8 * i);
        }
        for i in 0..=group.len() {
            out.push(ALPHABET[(packed >> (18 - 6 * i)) as usize & 0x3F] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_and_hmac_give_the_published_answers() {
        assert_eq!(
            hex_of(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex_of(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // RFC 4231, test cases 1 and 2.
        assert_eq!(
            hex_of(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex_of(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 4231, test case 6: a key longer than the block.
        assert_eq!(
            hex_of(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
