//! Sends a chain its workload and writes down what the chain answered.
//!
//! The load is a guest: the world starts it as one more node, so its network, clock and randomness
//! are the run's. It judges nothing. It submits the state's pre-signed transactions at a rate, asks
//! where each landed, asks each node its head, and after the fault window and the settle time asks
//! every node for every transaction a node said it committed. Each answer is one SDK event; the
//! world's chain model reads them. See `docs/ARCHITECTURE.md` §5.21.
//!
//! The plan is the one argument, a JSON document the tool derives from `[adapter]` and
//! `[workload]`.

#![forbid(unsafe_code)]
// A guest: it reads the clock and sleeps on purpose, and the world answers both.
#![allow(clippy::disallowed_methods)]

use std::{
    collections::BTreeMap,
    process::ExitCode,
    time::{Duration, Instant},
};

use bohr_sdk_proto::{
    json::{self, Json},
    rpc,
};

/// Longest a node may take to answer one call.
const WAIT: Duration = Duration::from_secs(1);

/// How long after submitting a transaction its inclusion is first asked.
const INCLUSION_AFTER: Duration = Duration::from_secs(1);

/// One call of the adapter: the method, its parameters with `{name}` holes, and where in the reply
/// each field the record needs is.
struct Call {
    method: String,
    params: Json,
    /// Field name to path in `{"result": …}`.
    fields: BTreeMap<String, String>,
    /// A field that must equal a value for the call to count: `(path, value)`.
    holds: Option<(String, Json)>,
}

/// Everything the tool told the load to do.
struct Plan {
    targets: Vec<(u32, u16)>,
    /// Every node of the chain: each is asked its head, and at the end every committed
    /// transaction.
    nodes: Vec<(u32, u16)>,
    txs: Vec<String>,
    every_tx: Duration,
    observe_every: Duration,
    window: Duration,
    settle: Duration,
    submit: Call,
    included: Call,
    head: Call,
    /// One block by its height, where the adapter declares it.
    block: Option<Call>,
}

/// What a node said of one committed transaction at the end.
enum Final {
    /// It holds it.
    Held,
    /// It answered, and does not hold it: its index has none and its block at the committed
    /// height has none.
    Absent,
    /// It did not answer, which says nothing about what it holds.
    Unanswered(String),
}

fn main() -> ExitCode {
    let Some(text) = std::env::args().nth(1) else {
        eprintln!("bohr-load: the plan is the one argument");
        return ExitCode::from(2);
    };
    let plan = match read_plan(&text) {
        Ok(plan) => plan,
        Err(why) => {
            eprintln!("bohr-load: the plan does not read: {why}");
            return ExitCode::from(2);
        }
    };
    if plan.targets.is_empty() {
        return ExitCode::SUCCESS;
    }
    run(&plan);
    ExitCode::SUCCESS
}

/// Submits, asks and observes until the window ends, then settles and asks every node.
fn run(plan: &Plan) {
    let started = Instant::now();
    let mut next_tx = 0usize;
    let mut tx_due = Duration::ZERO;
    let mut observe_due = Duration::ZERO;
    // Submitted and accepted, not yet found included: (tx, node, hash, when submitted).
    let mut waiting: Vec<(usize, u32, String, Duration)> = Vec::new();
    // Found committed: tx to its hash and the height it was found at.
    let mut committed: BTreeMap<usize, (String, i64)> = BTreeMap::new();
    loop {
        let now = started.elapsed();
        if now >= plan.window {
            break;
        }
        if next_tx < plan.txs.len() && now >= tx_due {
            let (node, port) = plan.targets[next_tx % plan.targets.len()];
            submit(plan, next_tx, node, port, now, &mut waiting);
            next_tx += 1;
            tx_due += plan.every_tx;
        }
        if now >= observe_due {
            for &(node, port) in &plan.nodes {
                observe(plan, node, port);
            }
            observe_due += plan.observe_every;
        }
        waiting.retain(|(tx, node, hash, at)| {
            if now < *at + INCLUSION_AFTER {
                return true;
            }
            let port = plan.targets.iter().find(|(n, _)| n == node).map_or(0, |(_, p)| *p);
            match included(plan, *node, port, hash) {
                Some((height, succeeded)) => {
                    send(
                        "load.committed",
                        &[
                            ("tx", Json::Int(*tx as i64)),
                            ("node", Json::Int(i64::from(*node))),
                            ("height", Json::Int(height)),
                            ("succeeded", Json::Bool(succeeded)),
                        ],
                    );
                    if succeeded {
                        committed.insert(*tx, (hash.clone(), height));
                    }
                    false
                }
                None => true,
            }
        });
        let next = [tx_due, observe_due].into_iter().min().unwrap_or(plan.window);
        let pause = next.saturating_sub(started.elapsed()).max(Duration::from_millis(1));
        std::thread::sleep(pause.min(Duration::from_millis(100)));
    }
    // The nodes are still watched while they settle, so one restarted late is seen answering.
    let settled = plan.window + plan.settle;
    while started.elapsed() < settled {
        if started.elapsed() >= observe_due {
            for &(node, port) in &plan.nodes {
                observe(plan, node, port);
            }
            observe_due += plan.observe_every;
        }
        let pause = observe_due.min(settled).saturating_sub(started.elapsed());
        std::thread::sleep(pause.max(Duration::from_millis(1)));
    }
    // Every node is asked for every transaction a node committed: a replica past the height that
    // answers and does not hold one has lost it.
    for &(node, port) in &plan.nodes {
        let head = observe(plan, node, port);
        for (tx, (hash, height)) in &committed {
            let mut fields =
                vec![("tx", Json::Int(*tx as i64)), ("node", Json::Int(i64::from(node)))];
            match settle_one(plan, node, port, *tx, hash, *height) {
                Final::Held => fields.push(("held", Json::Bool(true))),
                Final::Absent => fields.push(("held", Json::Bool(false))),
                Final::Unanswered(why) => fields.push(("unanswered", Json::Str(why))),
            }
            if let Some(head) = head {
                fields.push(("head", Json::Int(head)));
            }
            send("load.final", &fields);
        }
    }
}

/// Whether a node holds a committed transaction: by its index, and where the index has none, by
/// the block at the height it was committed at, compared byte for byte with what was sent.
fn settle_one(plan: &Plan, node: u32, port: u16, tx: usize, hash: &str, height: i64) -> Final {
    let encoded = base64(&unhex(hash));
    match ask(&plan.included, node, port, &[("hash", hash), ("hash_base64", &encoded)]) {
        Ok(fields) if fields.contains_key("height") => return Final::Held,
        Ok(_) => {}
        // The server answered with an error: it has no such transaction in its index.
        Err(why) if why.starts_with("error:") => {}
        Err(why) => return Final::Unanswered(why),
    }
    let Some(block) = &plan.block else { return Final::Absent };
    let at = height.to_string();
    match ask(block, node, port, &[("h", &at)]) {
        Ok(fields) => match fields.get("txs") {
            Some(Json::Array(txs)) if txs.iter().any(|t| t.as_str() == Some(&plan.txs[tx])) => {
                Final::Held
            }
            _ => Final::Absent,
        },
        Err(why) => Final::Unanswered(why),
    }
}

/// Submits one transaction and records whether the node accepted it.
fn submit(
    plan: &Plan,
    tx: usize,
    node: u32,
    port: u16,
    now: Duration,
    waiting: &mut Vec<(usize, u32, String, Duration)>,
) {
    let holes = [("tx_base64", plan.txs[tx].as_str())];
    let answer = ask(&plan.submit, node, port, &holes);
    let (accepted, hash) = match &answer {
        Ok(fields) => (
            fields.contains_key("accepted"),
            fields.get("hash").and_then(Json::as_str).map(str::to_owned),
        ),
        Err(_) => (false, None),
    };
    let mut record = vec![
        ("tx", Json::Int(tx as i64)),
        ("node", Json::Int(i64::from(node))),
        ("accepted", Json::Bool(accepted)),
    ];
    if let Some(hash) = &hash {
        record.push(("hash", Json::Str(hash.clone())));
    }
    if let Err(why) = &answer {
        record.push(("error", Json::Str(why.clone())));
    }
    send("load.submitted", &record);
    if let (true, Some(hash)) = (accepted, hash) {
        waiting.push((tx, node, hash, now));
    }
}

/// Asks a node where a transaction landed: its height and whether it succeeded, where it is held.
fn included(plan: &Plan, node: u32, port: u16, hash: &str) -> Option<(i64, bool)> {
    let encoded = base64(&unhex(hash));
    let fields =
        ask(&plan.included, node, port, &[("hash", hash), ("hash_base64", &encoded)]).ok()?;
    let height = fields.get("height").and_then(number)?;
    Some((height, fields.contains_key("accepted")))
}

/// Asks a node its head and records it; the height where it answered.
fn observe(plan: &Plan, node: u32, port: u16) -> Option<i64> {
    match ask(&plan.head, node, port, &[]) {
        Ok(fields) => {
            let height = fields.get("height").and_then(number);
            let mut record = vec![("node", Json::Int(i64::from(node)))];
            if let Some(height) = height {
                record.push(("height", Json::Int(height)));
            }
            send("load.head", &record);
            height
        }
        Err(why) => {
            send("load.head", &[("node", Json::Int(i64::from(node))), ("error", Json::Str(why))]);
            None
        }
    }
}

/// Makes one adapter call and reads the fields it names; `accepted` is present where the call's
/// condition held.
fn ask(
    call: &Call,
    node: u32,
    port: u16,
    holes: &[(&str, &str)],
) -> Result<BTreeMap<String, Json>, String> {
    let params = filled(&call.params, holes);
    let result = rpc::call(rpc::address(node, port), &call.method, &params, WAIT)?;
    let reply = Json::Object(BTreeMap::from([("result".to_owned(), result)]));
    let mut fields = BTreeMap::new();
    for (name, path) in &call.fields {
        if let Some(value) = rpc::at(&reply, path) {
            fields.insert(name.clone(), value.clone());
        }
    }
    let held = call.holds.as_ref().is_none_or(|(path, want)| rpc::at(&reply, path) == Some(want));
    if held {
        fields.insert("accepted".to_owned(), Json::Bool(true));
    }
    Ok(fields)
}

/// `template` with every `{name}` in its strings replaced by its value.
fn filled(template: &Json, holes: &[(&str, &str)]) -> Json {
    match template {
        Json::Str(text) => {
            let mut out = text.clone();
            for (name, value) in holes {
                out = out.replace(&format!("{{{name}}}"), value);
            }
            Json::Str(out)
        }
        Json::Array(items) => Json::Array(items.iter().map(|i| filled(i, holes)).collect()),
        Json::Object(map) => {
            Json::Object(map.iter().map(|(k, v)| (k.clone(), filled(v, holes))).collect())
        }
        other => other.clone(),
    }
}

/// Bytes from hex, upper or lower case; a stray character ends them.
fn unhex(text: &str) -> Vec<u8> {
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    text.as_bytes()
        .chunks(2)
        .map_while(|pair| Some(digit(pair[0])? << 4 | digit(*pair.get(1)?)?))
        .collect()
}

/// Bytes as standard base64 with padding, as a JSON-RPC body carries them.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A number written as a number or as decimal text, as CometBFT writes heights.
fn number(value: &Json) -> Option<i64> {
    match value {
        Json::Int(n) => Some(*n),
        Json::Str(text) => text.parse().ok(),
        _ => None,
    }
}

fn send(name: &str, fields: &[(&str, Json)]) {
    let details: BTreeMap<String, Json> =
        fields.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect();
    bohr_sdk::send_event(name, Json::Object(details));
}

/// Reads the plan the tool wrote; the transactions are read from the file it names.
fn read_plan(text: &str) -> Result<Plan, String> {
    let plan = json::parse(text)?;
    let int = |key: &str| {
        plan.get(key)
            .and_then(Json::as_int)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or(format!("no {key}"))
    };
    let endpoints = |key: &str| match plan.get(key) {
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                let node =
                    item.get("node").and_then(Json::as_int).ok_or("an endpoint without node")?;
                let port =
                    item.get("port").and_then(Json::as_int).ok_or("an endpoint without port")?;
                Ok((
                    u32::try_from(node).map_err(|e| e.to_string())?,
                    u16::try_from(port).map_err(|e| e.to_string())?,
                ))
            })
            .collect::<Result<Vec<_>, String>>(),
        _ => Err(format!("no {key}")),
    };
    let targets = endpoints("targets")?;
    let nodes = endpoints("nodes").unwrap_or_else(|_| targets.clone());
    let file = plan.get("txs").and_then(Json::as_str).ok_or("no txs")?;
    let txs: Vec<String> = std::fs::read_to_string(file)
        .map_err(|e| format!("{file}: {}", e.kind()))?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    let rate = int("rate")?.max(1);
    Ok(Plan {
        targets,
        nodes,
        txs,
        every_tx: Duration::from_nanos(1_000_000_000 / rate),
        observe_every: Duration::from_nanos(int("observe_every")?.max(1)),
        window: Duration::from_nanos(int("window")?),
        settle: Duration::from_nanos(int("settle")?),
        submit: read_call(plan.get("submit"))?,
        included: read_call(plan.get("included"))?,
        head: read_call(plan.get("head"))?,
        block: plan.get("block").map(|b| read_call(Some(b))).transpose()?,
    })
}

fn read_call(call: Option<&Json>) -> Result<Call, String> {
    let call = call.ok_or("a call is missing")?;
    let method = call.get("method").and_then(Json::as_str).ok_or("a call without method")?;
    let fields = match call.get("fields") {
        Some(Json::Object(map)) => map
            .iter()
            .filter_map(|(name, path)| Some((name.clone(), path.as_str()?.to_owned())))
            .collect(),
        _ => BTreeMap::new(),
    };
    let holds = call.get("holds").and_then(|holds| {
        Some((holds.get("field")?.as_str()?.to_owned(), holds.get("equals")?.clone()))
    });
    Ok(Call {
        method: method.to_owned(),
        params: call.get("params").cloned().unwrap_or(Json::Object(BTreeMap::new())),
        fields,
        holds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holes_are_filled_in_every_string_of_the_parameters() {
        let template = json::parse(r#"{"tx":"{tx_base64}","list":["0x{hash}",3]}"#).expect("json");
        let filled = filled(&template, &[("tx_base64", "AAA="), ("hash", "BEEF")]);
        assert_eq!(filled.to_json(), r#"{"list":["0xBEEF",3],"tx":"AAA="}"#);
    }

    #[test]
    fn a_call_reads_its_fields_and_its_condition() {
        let call = read_call(Some(
            &json::parse(
                r#"{"method":"tx","params":{},"fields":{"height":"result.height"},
                    "holds":{"field":"result.tx_result.code","equals":0}}"#,
            )
            .expect("json"),
        ))
        .expect("reads");
        assert_eq!(call.method, "tx");
        assert_eq!(call.fields.get("height").map(String::as_str), Some("result.height"));
        assert_eq!(
            call.holds.map(|(path, want)| (path, want.to_json())),
            Some(("result.tx_result.code".to_owned(), "0".to_owned()))
        );
        assert_eq!(number(&Json::Str("12".to_owned())), Some(12));
    }

    #[test]
    fn a_hex_hash_becomes_the_base64_a_json_rpc_body_carries() {
        assert_eq!(base64(&unhex("BFFE")), "v/4=");
        assert_eq!(base64(b"k1=v1"), "azE9djE=");
        assert_eq!(base64(&unhex("00ff10")), "AP8Q");
    }
}
