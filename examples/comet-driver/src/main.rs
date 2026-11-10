//! A client of a CometBFT network that holds it to the transactions it committed.
//!
//! It sends `k<i>=v<i>` to the kvstore application through `broadcast_tx_commit`, one validator
//! after another, and records each commit with its height. At the end it asks every validator for
//! every committed key: a validator whose application is at or past the commit height and does not
//! hold the value has lost a committed transaction.
//!
//! The record of commits is synced before the next send, so a kill of this node loses nothing it
//! acknowledged. A loss ends the process with `abort`, which the oracle reads first.
//!
//! It holds no model of CometBFT beyond the two JSON-RPC calls; the network judges itself.

use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    thread::sleep,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Validators in the network, at the addresses the world gives nodes 0 to 3.
const VALIDATORS: u8 = 4;

/// The port every validator answers JSON-RPC on.
const RPC_PORT: u16 = 26657;

/// Where this node records what the network committed.
const LOG_PATH: &str = "/driver/commits";

/// Where this node records when it first started, in nanoseconds of the wall clock.
const STARTED_PATH: &str = "/driver/started";

/// How long transactions are sent, in virtual time from the first start.
const SEND_FOR: Duration = Duration::from_secs(14);

/// How long the network is given to settle before it is checked.
const SETTLE: Duration = Duration::from_secs(4);

/// How long one answer may take before the request counts as unanswered.
const ANSWER_WITHIN: Duration = Duration::from_secs(4);

/// Largest answer read.
const MAX_ANSWER: usize = 64 * 1024;

/// The pause between two sends.
const PACE: Duration = Duration::from_millis(50);

/// One committed transaction: its index and the height it was committed at.
#[derive(Clone, Copy)]
struct Commit {
    index: u64,
    height: u64,
}

fn validator(n: u8) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(10, 66, 0, n + 1), RPC_PORT)
}

/// One HTTP GET, closing the connection; the body, or nothing where no whole answer came.
fn get(to: SocketAddrV4, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(to).ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {to}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    stream.set_nonblocking(true).ok()?;
    let started = Instant::now();
    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                answer.extend_from_slice(&chunk[..n]);
                if answer.len() > MAX_ANSWER {
                    return None;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if started.elapsed() > ANSWER_WITHIN {
                    return None;
                }
                sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
    let text = String::from_utf8_lossy(&answer).into_owned();
    text.find("\r\n\r\n").map(|at| text[at + 4..].to_owned())
}

/// The last quoted number after `"name":"` in `text`.
fn last_quoted_number(text: &str, name: &str) -> Option<u64> {
    let marker = format!("\"{name}\":\"");
    let at = text.rfind(&marker)? + marker.len();
    text[at..].split('"').next()?.parse().ok()
}

/// Whether every `"code":` in `text` is zero; an absent code is zero.
fn all_codes_zero(text: &str) -> bool {
    text.match_indices("\"code\":").all(|(at, m)| {
        let rest = &text[at + m.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.is_empty() || digits.parse::<u64>() == Ok(0)
    })
}

/// The first string value after `"name":"` in `text`.
fn quoted(text: &str, name: &str) -> Option<String> {
    let marker = format!("\"{name}\":\"");
    let at = text.find(&marker)? + marker.len();
    Some(text[at..].split('"').next()?.to_owned())
}

/// Standard base64 with padding, as CometBFT writes query values.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The wall clock's reading at this node's first start, recorded then and read on every restart.
///
/// The send window is measured from it, so a restart does not move the window past the horizon.
fn first_start() -> u128 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    if let Some(at) = std::fs::read_to_string(STARTED_PATH).ok().and_then(|t| t.trim().parse().ok())
    {
        return at;
    }
    if let Ok(mut file) = File::create(STARTED_PATH) {
        let _ = file.write_all(format!("{now}\n").as_bytes());
        let _ = file.sync_data();
    }
    now
}

/// How long ago this node first started, by the wall clock.
fn since(first: u128) -> Duration {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    Duration::from_nanos(u64::try_from(now.saturating_sub(first)).unwrap_or(u64::MAX))
}

/// The commits recorded before this node last died, and the log opened for more.
///
/// A kill can leave the last line cut, and the next line would be appended to it, so the log is
/// cut back to its last whole line before it is reopened.
fn reopen_log() -> (Vec<Commit>, File) {
    let _ = std::fs::create_dir_all("/driver");
    let mut text = std::fs::read_to_string(LOG_PATH).unwrap_or_default();
    let whole = text.rfind('\n').map_or(0, |at| at + 1);
    if whole < text.len() {
        text.truncate(whole);
        if let Ok(file) = OpenOptions::new().write(true).open(LOG_PATH) {
            let _ = file.set_len(whole as u64);
            let _ = file.sync_data();
        }
    }
    let commits = text
        .lines()
        .filter_map(|line| {
            let (index, height) = line.split_once(' ')?;
            Some(Commit { index: index.parse().ok()?, height: height.parse().ok()? })
        })
        .collect();
    let log = OpenOptions::new().create(true).append(true).open(LOG_PATH).expect("the commit log");
    (commits, log)
}

fn main() {
    let (mut commits, mut log) = reopen_log();
    let first = first_start();
    let mut next = commits.iter().map(|c| c.index + 1).max().unwrap_or(0);
    let (mut refused, mut unanswered) = (0u64, 0u64);

    while since(first) < SEND_FOR {
        let to = validator((next % u64::from(VALIDATORS)) as u8);
        let path = format!("/broadcast_tx_commit?tx=%22k{next}%3Dv{next}%22");
        match get(to, &path) {
            Some(body) if body.contains("\"result\"") && all_codes_zero(&body) => {
                match last_quoted_number(&body, "height") {
                    Some(height) if height > 0 => {
                        // One write, synced before the next send, so an acknowledged commit is never
                        // lost here.
                        let _ = log.write_all(format!("{next} {height}\n").as_bytes());
                        let _ = log.sync_data();
                        commits.push(Commit { index: next, height });
                    }
                    _ => refused += 1,
                }
            }
            Some(_) => refused += 1,
            None => unanswered += 1,
        }
        next += 1;
        sleep(PACE);
    }

    sleep(SETTLE);

    // Every committed key, asked of every validator that has applied its height. A validator that
    // does not answer is passed over, since each unanswered request costs ANSWER_WITHIN.
    let (mut checked, mut behind, mut lost) = (0u64, 0u64, Vec::new());
    for n in 0..VALIDATORS {
        for (asked, commit) in commits.iter().enumerate() {
            let path = format!("/abci_query?data=%22k{}%22", commit.index);
            let Some(body) = get(validator(n), &path) else {
                behind += (commits.len() - asked) as u64;
                break;
            };
            let applied = last_quoted_number(&body, "height").unwrap_or(0);
            if applied < commit.height {
                behind += 1;
                continue;
            }
            checked += 1;
            let expected = base64(format!("v{}", commit.index).as_bytes());
            if quoted(&body, "value").as_deref() != Some(expected.as_str()) {
                lost.push((n, *commit, applied));
            }
        }
    }

    println!(
        "comet-driver: committed {} refused {refused} unanswered {unanswered} checked {checked} \
         behind {behind} lost {}",
        commits.len(),
        lost.len()
    );
    for (n, commit, applied) in &lost {
        println!(
            "  validator {n} at height {applied} has no k{} committed at height {}",
            commit.index, commit.height
        );
    }
    if !lost.is_empty() {
        // A signal, so the oracle sees it whatever the validators exit with afterwards.
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b"v1"), "djE=");
        assert_eq!(base64(b"v10"), "djEw");
        assert_eq!(base64(b"v100"), "djEwMA==");
    }

    #[test]
    fn codes_are_read_as_numbers() {
        assert!(all_codes_zero(r#"{"check_tx":{"code":0},"tx_result":{}}"#));
        assert!(!all_codes_zero(r#"{"check_tx":{"code":2}}"#));
    }

    #[test]
    fn the_commit_height_is_the_last_one() {
        let body = r#"{"result":{"check_tx":{},"tx_result":{},"hash":"AB","height":"23"}}"#;
        assert_eq!(last_quoted_number(body, "height"), Some(23));
    }
}
