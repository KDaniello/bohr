//! Asks the nodes of a cluster what they did, over their own JSON-RPC, and writes down the answers.
//!
//! **A guest, not a part of the supervisor.** It is started by the world as one more node, so its
//! sockets are the virtual network's, its clock is the run's and its randomness is the tape's: a
//! probe that asked from outside would see a cluster at moments the run never chose, and two runs
//! of one seed would be asked different questions. See `docs/ARCHITECTURE.md` §5.17.
//!
//! It judges nothing. Each round it takes the replies to what it asked last round, asks again,
//! and writes one SDK event with what came back or why nothing did. The world compares.
//!
//! The plan is the one argument, a JSON document the tool derives from the scenario's probes:
//! `{"requests":[{"node":0,"port":8545,"method":"…","params":[…],"fields":["number"],"every":N}]}`.

#![forbid(unsafe_code)]
// The probe is a guest: it sleeps on purpose, and the world answers the sleep.
#![allow(clippy::disallowed_methods)]

use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    process::ExitCode,
    time::Duration,
};

use bohr_sdk_proto::json::{self, Json};

/// The SDK event a round is reported under; the world reads only the probe node's.
const PROBE_EVENT: &str = "bohr_probe";

/// Longest a node may take to answer: three beats, and never more than a second.
const MOST_WAIT: Duration = Duration::from_secs(1);

/// Largest reply read; a larger one is reported as too large rather than held.
const MOST_BODY: usize = 1 << 20;

/// One call, as the plan names it.
struct Request {
    node: u32,
    port: u16,
    method: String,
    params: Json,
    fields: Vec<String>,
    every: Duration,
}

/// What one round learned: per request, the result or why there is none.
type Answers = Vec<(usize, Result<Json, String>)>;

fn main() -> ExitCode {
    let Some(plan) = std::env::args().nth(1) else {
        eprintln!("bohr-probe: the plan is the one argument");
        return ExitCode::from(2);
    };
    let requests = match read_plan(&plan) {
        Ok(requests) => requests,
        Err(why) => {
            eprintln!("bohr-probe: the plan does not read: {why}");
            return ExitCode::from(2);
        }
    };
    if requests.is_empty() {
        return ExitCode::SUCCESS;
    }

    // Rounds on one beat, the shortest interval any request asked for; a request with a longer one
    // is asked every so many rounds. Counted in rounds rather than read off the clock, so a round
    // costs the calls that ask and nothing else.
    let beat = requests.iter().map(|r| r.every).min().unwrap_or(MOST_WAIT);
    let rounds_in = |span: Duration| span.as_nanos() / beat.as_nanos().max(1);
    let period: Vec<u128> = requests.iter().map(|r| rounds_in(r.every).max(1)).collect();
    let patience = rounds_in(MOST_WAIT).clamp(1, 3);
    let mut draws = Draws::seeded();
    let mut endpoints: BTreeMap<(u32, u16), Endpoint> = BTreeMap::new();
    let mut round: u128 = 0;
    loop {
        std::thread::sleep(draws.jittered(beat));
        let mut answered = Answers::new();

        // What earlier rounds asked: whatever has arrived, without waiting for what has not.
        for endpoint in endpoints.values_mut() {
            endpoint.collect(patience, &mut answered);
        }

        // What this round asks: one batch per node, to every node not still owing an answer.
        let mut batches: BTreeMap<(u32, u16), Vec<usize>> = BTreeMap::new();
        for (index, request) in requests.iter().enumerate() {
            if round.is_multiple_of(period[index]) {
                batches.entry((request.node, request.port)).or_default().push(index);
            }
        }
        for (key, members) in batches {
            let endpoint = endpoints.entry(key).or_default();
            if endpoint.owed.is_empty() {
                let body = batch_body(&requests, &members);
                endpoint.ask(key, &body, members, &mut answered);
            }
        }

        if !answered.is_empty() {
            answered.sort_by_key(|(index, _)| *index);
            report(&requests, answered);
        }
        round += 1;
    }
}

/// Writes one round as one SDK event.
fn report(requests: &[Request], answered: Answers) {
    let items = answered
        .into_iter()
        .map(|(index, answer)| {
            let mut item = BTreeMap::new();
            item.insert("request".to_owned(), Json::Int(index as i64));
            match answer {
                Ok(result) => {
                    item.insert("result".to_owned(), reduced(&result, &requests[index].fields))
                }
                Err(why) => item.insert("unanswered".to_owned(), Json::Str(why)),
            };
            Json::Object(item)
        })
        .collect();
    let mut details = BTreeMap::new();
    details.insert("round".to_owned(), Json::Array(items));
    bohr_sdk::send_event(PROBE_EVENT, Json::Object(details));
}

/// Reads the plan the tool wrote.
fn read_plan(text: &str) -> Result<Vec<Request>, String> {
    let plan = json::parse(text)?;
    let Some(Json::Array(items)) = plan.get("requests") else {
        return Err("no requests".to_owned());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let int = |key: &str| item.get(key).and_then(Json::as_int).ok_or(format!("no {key}"));
        let node = u32::try_from(int("node")?).map_err(|e| e.to_string())?;
        let port = u16::try_from(int("port")?).map_err(|e| e.to_string())?;
        let every = u64::try_from(int("every")?).map_err(|e| e.to_string())?;
        let method = item.get("method").and_then(Json::as_str).ok_or("no method")?;
        let fields = match item.get("fields") {
            Some(Json::Array(fields)) => {
                fields.iter().filter_map(Json::as_str).map(str::to_owned).collect()
            }
            _ => Vec::new(),
        };
        out.push(Request {
            node,
            port,
            method: method.to_owned(),
            params: item.get("params").cloned().unwrap_or(Json::Array(Vec::new())),
            fields,
            every: Duration::from_nanos(every.max(1)),
        });
    }
    Ok(out)
}

/// The run's randomness, drawn once from the tape and stretched.
///
/// One draw at start rather than one a round: the seed is the tape's, so every moment the probe
/// chooses is still the run's, and a round is spared a call.
struct Draws(u64);

impl Draws {
    /// Seeded from `/dev/urandom`, which the world answers from the tape.
    fn seeded() -> Self {
        let mut bytes = [0u8; 8];
        let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
        let seed = if read.is_ok() { u64::from_le_bytes(bytes) } else { 0 };
        Self(seed | 1)
    }

    /// xorshift64*: small, and enough to spread moments, which is all it is asked to do.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// An interval moved by up to a quarter either way.
    ///
    /// Moved so that two seeds ask at different moments: a probe on a fixed beat would only ever
    /// see the cluster at one phase of whatever rhythm it keeps.
    fn jittered(&mut self, every: Duration) -> Duration {
        let draw = u128::from(self.next() >> 48);
        let nanos = every.as_nanos();
        let moved = nanos * 3 / 4 + nanos / 2 * draw / 65_536;
        Duration::from_nanos(u64::try_from(moved).unwrap_or(u64::MAX))
    }
}

/// One node's JSON-RPC endpoint: the connection, and the answers it still owes.
#[derive(Default)]
struct Endpoint {
    stream: Option<TcpStream>,
    /// The requests in the batch sent and not yet answered.
    owed: Vec<usize>,
    /// What has arrived of the reply so far.
    buf: Vec<u8>,
    /// Rounds the owed answers have waited.
    waited: u128,
}

impl Endpoint {
    /// Sends one batch, connecting first where there is no connection.
    fn ask(&mut self, key: (u32, u16), body: &str, members: Vec<usize>, answered: &mut Answers) {
        if self.stream.is_none() {
            let connected = TcpStream::connect(address(key.0, key.1)).and_then(|stream| {
                stream.set_nonblocking(true)?;
                Ok(stream)
            });
            match connected {
                Ok(stream) => self.stream = Some(stream),
                Err(e) => {
                    let why = format!("connect: {}", e.kind());
                    answered.extend(members.into_iter().map(|i| (i, Err(why.clone()))));
                    return;
                }
            }
        }
        let Some(stream) = self.stream.as_mut() else { return };
        match send(stream, key, body) {
            Ok(()) => {
                self.owed = members;
                self.waited = 0;
                self.buf.clear();
            }
            Err(why) => {
                self.stream = None;
                answered.extend(members.into_iter().map(|i| (i, Err(why.clone()))));
            }
        }
    }

    /// Takes the reply to the batch owed, where it has arrived whole.
    ///
    /// Gives up after `patience` rounds and drops the connection then: a reply that came later
    /// would be read as the answer to the next batch.
    fn collect(&mut self, patience: u128, answered: &mut Answers) {
        if self.owed.is_empty() {
            return;
        }
        let outcome = match self.stream.as_mut() {
            Some(stream) => {
                read_available(stream, &mut self.buf).and_then(|()| parse_reply(&self.buf))
            }
            None => Err("closed".to_owned()),
        };
        let owed = std::mem::take(&mut self.owed);
        let fail = |answered: &mut Answers, owed: Vec<usize>, why: &str| {
            answered.extend(owed.into_iter().map(|i| (i, Err(why.to_owned()))));
        };
        match outcome {
            Ok(Some((reply, keep))) => {
                if !keep {
                    self.stream = None;
                }
                match json::parse(&reply) {
                    Ok(reply) => answered.extend(owed.iter().map(|i| (*i, answer_for(&reply, *i)))),
                    Err(e) => fail(answered, owed, &format!("unreadable: {e}")),
                }
            }
            Ok(None) if self.waited + 1 < patience => {
                self.waited += 1;
                self.owed = owed;
            }
            Ok(None) => {
                self.stream = None;
                fail(answered, owed, "timeout");
            }
            Err(why) => {
                self.stream = None;
                fail(answered, owed, &why);
            }
        }
    }
}

/// The body of one node's batch: the calls, each with its request's index as its id.
///
/// A single call goes alone rather than as a batch of one, which every JSON-RPC server answers.
fn batch_body(requests: &[Request], members: &[usize]) -> String {
    let call = |index: usize| {
        let request = &requests[index];
        let mut call = BTreeMap::new();
        call.insert("jsonrpc".to_owned(), Json::Str("2.0".to_owned()));
        call.insert("id".to_owned(), Json::Int(index as i64));
        call.insert("method".to_owned(), Json::Str(request.method.clone()));
        call.insert("params".to_owned(), request.params.clone());
        Json::Object(call)
    };
    match members {
        [one] => call(*one).to_json(),
        many => Json::Array(many.iter().map(|i| call(*i)).collect()).to_json(),
    }
}

/// One call's result in a reply, found by its id where the reply is a batch.
fn answer_for(reply: &Json, index: usize) -> Result<Json, String> {
    let item = match reply {
        Json::Array(items) => items
            .iter()
            .find(|item| item.get("id").and_then(Json::as_int) == Some(index as i64))
            .ok_or_else(|| "missing from the batch".to_owned())?,
        single => single,
    };
    if let Some(error) = item.get("error") {
        let message = error.get("message").and_then(Json::as_str).unwrap_or("error");
        return Err(format!("error: {message}"));
    }
    Ok(item.get("result").cloned().unwrap_or(Json::Null))
}

/// The address the world gives node `index`.
fn address(index: u32, port: u16) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::from(0x0A42_0000 | (index + 1)), port)
}

/// Writes one HTTP request.
fn send(stream: &mut TcpStream, key: (u32, u16), body: &str) -> Result<(), String> {
    let head = format!(
        "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        address(key.0, key.1),
        body.len()
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    stream.write_all(&bytes).map_err(|e| format!("send: {}", e.kind()))
}

/// Reads whatever has arrived, without waiting.
fn read_available(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<(), String> {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Err("closed".to_owned()),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MOST_BODY {
                    return Err("reply too large".to_owned());
                }
                // A short read is everything there was; only a full one may have more behind it.
                if n < chunk.len() {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(format!("receive: {}", e.kind())),
        }
    }
}

/// One HTTP reply from what has arrived, with whether the connection may be used again; `None`
/// while some of it has still to come.
fn parse_reply(buf: &[u8]) -> Result<Option<(String, bool)>, String> {
    let Some(header_end) = find(buf, b"\r\n\r\n").map(|at| at + 4) else { return Ok(None) };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let status = head.split_whitespace().nth(1).unwrap_or("").to_owned();
    let keep = !head.contains("\r\nconnection: close");
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|n| n.trim().parse::<usize>().ok());
    let chunked =
        head.lines().any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));
    let raw = &buf[header_end..];
    let body = if chunked {
        match dechunk(raw)? {
            Some(body) => body,
            None => return Ok(None),
        }
    } else {
        let Some(length) = length else {
            return Err("reply has neither a length nor chunks".to_owned());
        };
        if raw.len() < length {
            return Ok(None);
        }
        raw[..length].to_vec()
    };
    if status != "200" {
        return Err(format!("http {status}"));
    }
    let text = String::from_utf8(body).map_err(|_| "reply is not text".to_owned())?;
    Ok(Some((text, keep)))
}

/// A chunked body, where every chunk to the last has arrived.
fn dechunk(mut raw: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut body = Vec::new();
    loop {
        let Some(line_end) = find(raw, b"\r\n") else { return Ok(None) };
        let size_text = String::from_utf8_lossy(&raw[..line_end]).to_string();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "a chunk size does not read".to_owned())?;
        if raw.len() < line_end + 2 + size + 2 {
            return Ok(None);
        }
        body.extend_from_slice(&raw[line_end + 2..line_end + 2 + size]);
        raw = &raw[line_end + 2 + size + 2..];
        if size == 0 {
            return Ok(Some(body));
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A result with only the named fields kept: per element for an array, as it is otherwise.
///
/// A dotted field keeps its nesting, so the world reads it back by the same path. `.` keeps the
/// whole value.
fn reduced(result: &Json, fields: &[String]) -> Json {
    match result {
        Json::Array(items) => Json::Array(items.iter().map(|i| reduced(i, fields)).collect()),
        Json::Object(_) if !fields.iter().any(|f| f == ".") => {
            let mut out = Json::Object(BTreeMap::new());
            for field in fields {
                if let Some(value) = at(result, field) {
                    put(&mut out, field, value.clone());
                }
            }
            out
        }
        other => other.clone(),
    }
}

fn at<'a>(item: &'a Json, path: &str) -> Option<&'a Json> {
    path.split('.').try_fold(item, |at, part| at.get(part))
}

fn put(into: &mut Json, path: &str, value: Json) {
    let mut parts = path.split('.').peekable();
    let mut here = into;
    while let Some(part) = parts.next() {
        let Json::Object(map) = here else { return };
        if parts.peek().is_none() {
            map.insert(part.to_owned(), value);
            return;
        }
        here = map.entry(part.to_owned()).or_insert_with(|| Json::Object(BTreeMap::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_keeps_only_what_the_plan_names() {
        let result = json::parse(
            r#"{"number":"0x5","hash":"0xaa","transactions":["0x1"],"a":{"b":1,"c":2}}"#,
        )
        .expect("json");
        let kept = reduced(&result, &["number".to_owned(), "a.b".to_owned()]);
        assert_eq!(kept.to_json(), r#"{"a":{"b":1},"number":"0x5"}"#);
        let list = Json::Array(vec![result.clone(), Json::Null]);
        assert_eq!(reduced(&list, &["hash".to_owned()]).to_json(), r#"[{"hash":"0xaa"},null]"#);
    }

    #[test]
    fn the_plan_the_tool_writes_reads_back_and_batches_by_node() {
        let plan = r#"{"requests":[
            {"node":1,"port":8545,"method":"eth_getBlockByNumber","params":["finalized",false],"fields":["number","hash"],"every":50000000},
            {"node":1,"port":8545,"method":"chain_votes","params":[],"fields":["hash"],"every":50000000}]}"#;
        let requests = read_plan(plan).expect("reads");
        assert_eq!(requests.len(), 2);
        assert_eq!(address(1, 8545).to_string(), "10.66.0.2:8545");
        let body = batch_body(&requests, &[0, 1]);
        assert!(body.starts_with('[') && body.contains(r#""id":1"#), "{body}");
        assert!(!batch_body(&requests, &[1]).starts_with('['));
    }

    #[test]
    fn a_reply_is_taken_only_once_whole() {
        let whole = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nnull";
        assert_eq!(parse_reply(&whole[..20]), Ok(None));
        assert_eq!(parse_reply(&whole[..whole.len() - 1]), Ok(None));
        assert_eq!(parse_reply(whole), Ok(Some(("null".to_owned(), true))));
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nnu\r\n2\r\nll\r\n0\r\n\r\n";
        assert_eq!(parse_reply(&chunked[..chunked.len() - 3]), Ok(None));
        assert_eq!(parse_reply(chunked), Ok(Some(("null".to_owned(), true))));
    }

    #[test]
    fn a_batch_reply_is_read_by_id_in_any_order() {
        let reply =
            json::parse(r#"[{"id":1,"result":[]},{"id":0,"error":{"code":-1,"message":"no"}}]"#)
                .expect("json");
        assert_eq!(answer_for(&reply, 1), Ok(Json::Array(Vec::new())));
        assert_eq!(answer_for(&reply, 0), Err("error: no".to_owned()));
        assert!(answer_for(&reply, 7).is_err());
    }

    #[test]
    fn the_draws_stay_within_a_quarter_of_the_beat() {
        let mut draws = Draws(12_345);
        let beat = Duration::from_millis(60);
        for _ in 0..1000 {
            let moved = draws.jittered(beat);
            assert!(moved >= beat * 3 / 4 && moved <= beat * 5 / 4, "{moved:?}");
        }
    }
}
