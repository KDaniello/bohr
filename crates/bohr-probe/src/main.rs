//! Asks the nodes of a cluster what they did, over their own JSON-RPC, and writes down the answers.
//!
//! The probe is a guest: the world starts it as one more node, so its network, clock and randomness
//! are the run's and one seed asks the same questions. See `docs/ARCHITECTURE.md` §5.17.
//!
//! It judges nothing. Each round it collects last round's replies, asks again, and writes one SDK
//! event with the answers or why there were none. The world compares.
//!
//! The plan is the one argument, a JSON document the tool derives from the scenario's probes:
//! `{"requests":[{"node":0,"port":8545,"method":"…","params":[…],"fields":["number"],"every":N}]}`.

#![forbid(unsafe_code)]
// The probe is a guest: it sleeps on purpose, and the world answers the sleep.
#![allow(clippy::disallowed_methods)]

use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    net::TcpStream,
    process::ExitCode,
    time::Duration,
};

use bohr_sdk_proto::{
    json::{self, Json},
    rpc::{MOST_BODY, address, answer_for, at, parse_reply, request_bytes},
};

/// Longest a node may take to answer: three beats, and never more than a second.
const MOST_WAIT: Duration = Duration::from_secs(1);

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

    // One beat is the shortest requested interval; longer ones are counted in rounds.
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

/// Writes one round as one SDK event, the world reading only the probe node's.
fn report(requests: &[Request], answered: Answers) {
    let round = bohr_sdk_proto::probe::Round(
        answered
            .into_iter()
            .map(|(index, answer)| {
                let answer = match answer {
                    Ok(result) => bohr_sdk_proto::probe::Answer::Result(reduced(
                        &result,
                        &requests[index].fields,
                    )),
                    Err(why) => bohr_sdk_proto::probe::Answer::Unanswered(why),
                };
                (index, answer)
            })
            .collect(),
    );
    bohr_sdk::send_event(bohr_sdk_proto::probe::PROBE_EVENT, round.to_details());
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
/// Drawn once at start; the seed is the tape's, so every chosen moment is still the run's.
struct Draws(u64);

impl Draws {
    /// Seeded from `/dev/urandom`, which the world answers from the tape.
    fn seeded() -> Self {
        let mut bytes = [0u8; 8];
        let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
        let seed = if read.is_ok() { u64::from_le_bytes(bytes) } else { 0 };
        Self(seed | 1)
    }

    /// xorshift64*, enough to spread moments.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// An interval moved by up to a quarter either way.
    ///
    /// Jitter lets different seeds sample different phases of the cluster's rhythm.
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
    /// Gives up after `patience` rounds and drops the connection, so a late reply is never read
    /// as the next batch's.
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
                    Ok(reply) => {
                        answered.extend(owed.iter().map(|i| (*i, answer_for(&reply, *i as i64))))
                    }
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
/// A single call is sent unbatched, which every JSON-RPC server accepts.
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

/// Writes one HTTP request.
fn send(stream: &mut TcpStream, key: (u32, u16), body: &str) -> Result<(), String> {
    stream
        .write_all(&request_bytes(address(key.0, key.1), body))
        .map_err(|e| format!("send: {}", e.kind()))
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
                // A short read drained the socket.
                if n < chunk.len() {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(format!("receive: {}", e.kind())),
        }
    }
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
    fn the_draws_stay_within_a_quarter_of_the_beat() {
        let mut draws = Draws(12_345);
        let beat = Duration::from_millis(60);
        for _ in 0..1000 {
            let moved = draws.jittered(beat);
            assert!(moved >= beat * 3 / 4 && moved <= beat * 5 / 4, "{moved:?}");
        }
    }
}
