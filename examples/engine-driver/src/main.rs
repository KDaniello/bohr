//! The other half of an OP Stack node: the thing that drives the execution client.
//!
//! An execution client does nothing on its own. It opens its store, stands up its engine API and
//! waits to be told what the chain is — and until somebody tells it, a run against it is a run
//! against a program that is idling. This is the caller: it authenticates the way a consensus
//! client does, and then drives the client the way one in sequencer mode does.
//!
//! **It never builds a block.** The loop asks the client to start building, takes back whatever it
//! produced, hands that same text straight back to be validated, and then names the hash the client
//! itself returned as the new head. Nothing here parses a payload, computes a root, or knows what a
//! transaction is — the driver moves opaque text the client gave it, which is what keeps a caller
//! for an execution client free of any model of the protocol it carries.
//!
//! What it does keep is what an oracle is made of: every hash the client called `VALID`, and the
//! state root of the head, left in a file for a projection to read.
//!
//! The scenario beside it names one program per node, which is what a cluster of unlike nodes is.
//! An earlier attempt had this binary replace itself with the client on node zero, and that is not
//! usable: a guest that replaces itself is no longer fully intercepted, and the client that stamps
//! its log with the world's clock when the supervisor starts it stamps the host's own date when it
//! arrives that way.

use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddrV4, TcpStream},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Where the shared secret is, on every node's filesystem.
///
/// A file rather than a message: the node reads it at startup and the caller reads it before its
/// first request, and neither is told about the other. Both nodes are given the same one because
/// the scenario puts it in the state every node starts from.
const SECRET_PATH: &str = "/jwt.hex";

/// The authenticated endpoint, which is the one a consensus client uses.
const ENGINE_PORT: u16 = 8551;
/// The open one, which needs no token and is the cheapest proof the path works at all.
const PUBLIC_PORT: u16 = 8545;

/// How long to keep trying before giving up on the node ever listening.
const ATTEMPTS: u32 = 400;
/// How long to wait between attempts, in virtual time.
const RETRY: Duration = Duration::from_millis(5);

/// Largest answer read back. An engine reply is a few hundred bytes; the capability list is the
/// longest of them and is well inside this.
const MAX_ANSWER: usize = 64 * 1024;

/// How many blocks the client is asked to build.
const BLOCKS: u32 = 4;

/// How many the branch after a rewind is.
const BRANCH: u32 = 2;

/// Seconds between one block and the next, which is what an OP Stack chain runs at.
const BLOCK_TIME: u64 = 2;

/// Where the head's state root is left, for a projection to compare implementations by.
///
/// At the root of the filesystem because a file cannot be made where there is nowhere to put it,
/// and a driver that had to make its own directory first would be testing `mkdir`.
const STATE_ROOT_PATH: &str = "/state-root";

/// Thirty-two zero bytes, which is what a caller with no beacon chain behind it has to offer.
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
/// **Not a finding, and the exit code is the whole of the reason.** A node killed before it could
/// answer has broken no promise, because it made none; a caller that exited non-zero over it would
/// have every crash landing during a startup counted as a defect in the client. What such a run
/// costs is a question left unasked, and the counts say so by being nought.
fn nothing_asked(why: &str) -> i32 {
    println!("nothing asked: {why}");
    println!("acknowledged 0 verified 0 forked 0 behind 0 unanswered 0");
    0
}

/// Asks the execution client what it is, what it can do, and where its chain begins.
///
/// Everything here is asked rather than assumed. Which engine methods exist, which block the chain
/// starts at and what that block was stamped with all decide how a caller must talk to this client,
/// and all three differ per chain — so a driver that wrote them down would be right about one chain
/// and quietly wrong about the next.
fn drive() -> i32 {
    let secret = match read_secret() {
        Ok(secret) => secret,
        Err(e) => {
            eprintln!("cannot read {SECRET_PATH}: {e}");
            return 2;
        }
    };

    // The open endpoint first, because it needs no token: an answer here says the connection, the
    // request and the parsing are all sound, so a failure at the authenticated one afterwards is
    // about the token and nothing else.
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

    // The block the chain starts at, which is the only head a caller can legitimately name before
    // it has built anything. Its stamp decides which fork is in force at the block after it, and so
    // which spelling of every engine call applies.
    let Some(genesis) = call(PUBLIC_PORT, None, "eth_getBlockByNumber", r#"["0x0",false]"#) else {
        return nothing_asked("the node stopped answering before it named its genesis");
    };
    let Some(head) = field(&genesis, "hash") else {
        return nothing_asked(&format!("no genesis block: {}", body(&genesis, 300)));
    };
    let timestamp = field(&genesis, "timestamp").unwrap_or_default();
    let gas_limit = field(&genesis, "gasLimit").unwrap_or_default();
    println!("genesis {head} timestamp {timestamp} gasLimit {gas_limit}");

    // Pointed at the block the client already has, which is the one forkchoice a caller with no
    // history of its own can make.
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

    // A block that cannot be built is not a finding. The node may be dead at this moment, and a
    // caller that treated silence as a broken promise would accuse a client of losing what it was
    // never able to say it had.
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

    // History rewound to the newest block the client was told is final, and a different one built
    // from there. Legal by construction — nothing final is stepped past — which is what makes the
    // check afterwards about the client rather than about the caller.
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

    // Written last and once, because what a projection compares is the state of the chain the run
    // left behind rather than any step on the way to it.
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
    /// The head's state root, which is the value two implementations of one specification must
    /// agree on and the only thing here worth comparing across them.
    state_root: String,
    /// What each block is allowed to spend, carried from genesis because an OP client requires the
    /// caller to name it and the chain it was started with is the only honest source for one.
    gas_limit: String,
    /// What the head was stamped with, which the next block has to be built past.
    stamp: u64,
}

/// What became of the promises, once the client has been asked about every one of them.
#[derive(Default)]
struct Verdict {
    /// Heights where the client still has the block it produced and was told is final.
    verified: u32,
    /// Heights where it has a *different* block. Each one is a finding.
    forked: Vec<(u32, String, String)>,
    /// Heights it has no block at, because it came back behind what it had answered for.
    ///
    /// Counted and never used to accuse. A client that keeps recent blocks in memory and persists
    /// them on a schedule of its own loses the unpersisted ones to a crash, and the consensus
    /// client it serves is expected to drive it forward again — so this is the ordinary cost of a
    /// crash rather than a broken promise.
    behind: u32,
    /// Questions the client never answered, because it was dead or unreachable when they were
    /// asked. An operation whose outcome is unknown is not an operation that failed.
    unanswered: u32,
}

/// Holds the client to what it said, after whatever the run did to it.
///
/// The question is asked by *height* and not by hash, and that is the whole of the sharpness here.
/// Asking whether a hash still exists cannot tell a client that fell behind from one that rewrote
/// history, and only the second is a fault: an execution client is entitled to lose blocks it had
/// not yet written down, and is never entitled to have a different one where a final block stood.
///
/// Every hash in the comparison came from the client itself, so nothing here knows what a block is.
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
            // An answer the caller could not read is not an answer about the chain.
            None => verdict.unanswered += 1,
        }
    }
    verdict
}

/// Rewinds to the newest final block and builds a different chain from it.
///
/// Different by its stamp and nothing else: one second later than the block it replaces, which is
/// enough to give it a hash of its own without the caller having to put anything in it. That is a
/// reorg as a client experiences one — the head it was working from is replaced by a sibling.
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
/// Start building, take what was built, hand it back to be validated, and name it the head. The
/// payload crosses this function as text and is never looked into except for the three fields the
/// next call needs — and each of those is a value the client itself produced.
fn build_one(token: &str, chain: &mut Chain, number: u32) -> Result<(), String> {
    // `noTxPool` because a caller that let the client draw on its pool would be building a
    // different block each time for reasons outside the run, and a run has to be a function of its
    // tape. The fee recipient and the randomness are zero for the same reason.
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
    // V1 answers with the payload itself and later spellings wrap it, so the wrapper is taken where
    // there is one. Either way what comes out is handed back untouched.
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

    // Finality lags the head by one block, which is what gives a crash something to contradict: a
    // client that comes back below what it called final has broken a promise nobody asked it to
    // make lightly.
    let previous = std::mem::replace(&mut chain.head, hash);
    let accepted = forkchoice(token, &chain.head, &previous, &previous, None)
        .ok_or_else(|| "the node stopped answering at the final forkchoice".to_owned())?;
    if !accepted.contains("\"VALID\"") {
        return Err(format!("the client refused its own new head: {}", body(&accepted, 300)));
    }
    // Recorded only once the client has accepted the forkchoice that names it final. A promise the
    // client never acknowledged is not a promise, and holding it to one would be the caller's error
    // reported as the client's. Never twice: a rewind names a block final that already was.
    chain.finalized = previous.clone();
    if chain.promised.last().map(|(_, held)| held) != Some(&previous) {
        chain.promised.push((number.saturating_sub(1), previous));
    }
    chain.state_root = root;
    Ok(())
}

/// One `engine_forkchoiceUpdated`, with attributes where the caller is asking for a block.
///
/// V1 throughout, because this chain's genesis is stamped before the fork that brought withdrawals
/// and every block built two seconds after the last stays there. The client offers every spelling
/// and accepts a forkchoice to genesis in all of them, so the choice is about the attributes.
fn forkchoice(
    token: &str,
    head: &str,
    safe: &str,
    finalized: &str,
    attributes: Option<&str>,
) -> Option<String> {
    // A hash the caller has not got yet is sent as zero, which is what "nothing is final" is
    // spelled as and what a chain at its genesis has to say.
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
/// The clock is the one call the tool forbids itself and this program is entitled to: a guest is
/// where the interception happens, so what comes back here is the world's time and not the host's.
/// A token stamped from anywhere else would be rejected as stale by a node living in 1970.
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
/// Retrying rather than failing, because the node takes as long as it takes to stand up and the
/// caller has no way to be told when it has. A refusal to connect is not an answer.
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
        // Read to the end of the stream: the request asked for the connection to be closed after
        // the answer, so the end of the stream is the end of the answer and no framing is needed.
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

/// One field of a JSON object, as the text between the quotes after its name. Enough for an answer
/// whose shape is known and never a parser: what is under test is the exchange, not the JSON.
fn field(answer: &str, name: &str) -> Option<String> {
    let at = answer.find(&format!("\"{name}\""))? + name.len() + 3;
    let rest = answer.get(at..)?.trim_start_matches([':', ' ', '"']);
    let end = rest.find(['"', ',', '}'])?;
    Some(rest[..end].to_owned())
}

/// The object a named field holds, as the text between its braces.
///
/// Brace matching and never a parser, because what comes out is handed straight back to the client
/// in the next call: the driver moves a payload without ever having to understand one. Quoted text
/// is skipped so a brace inside a string cannot close the object early.
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
/// Bounded by the caller, because what is worth reading differs by answer: a refusal is a sentence
/// and a capability list is every method the engine has.
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
