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

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddrV4, TcpStream},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use sha2::Sha256;

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

/// Largest answer read back; the capability list, the longest, fits easily.
const MAX_ANSWER: usize = 64 * 1024;

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
    std::process::exit(drive());
}

/// The node's address, which is the one the world gave node zero.
fn node(port: u16) -> SocketAddrV4 {
    SocketAddrV4::new([10, 66, 0, 1].into(), port)
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
        return nothing_asked(&format!("the client refused its own genesis: {}", body(&answer, 300)));
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
    let issued =
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let header = base64url(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = base64url(format!(r#"{{"iat":{issued}}}"#).as_bytes());
    let signed = format!("{header}.{claims}");

    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("any length of key is accepted");
    mac.update(signed.as_bytes());
    format!("{signed}.{}", base64url(&mac.finalize().into_bytes()))
}

/// Sends one request and reads the answer, retrying while nobody is listening yet.
///
/// Retries until the node listens.
fn call(port: u16, token: Option<&str>, method: &str, params: &str) -> Option<String> {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#);
    let authorization =
        token.map_or_else(String::new, |token| format!("Authorization: Bearer {token}\r\n"));
    let request = format!(
        "POST / HTTP/1.1\r\nHost: 10.66.0.1\r\nContent-Type: application/json\r\n\
         {authorization}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    for _ in 0..ATTEMPTS {
        let Ok(mut stream) = TcpStream::connect(node(port)) else {
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
