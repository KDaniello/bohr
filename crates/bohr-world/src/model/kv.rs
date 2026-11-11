//! A store's records, as the laws' vocabulary.
//!
//! The harness that drives a store writes what it began, what the store acknowledged, what it read
//! back and when it first answered after a start. Schema in `docs/API.md` §6.5:
//!
//! - `kv.begun` and `kv.acked`: `op` (`put`, `delete` or `batch`), `keys`, `values` (`null` for a
//!   removal), `seq` (below 2³², ordering the start's operations), and optionally `at`, `{file,
//!   line}` of the acknowledgement;
//! - `kv.read`: `key`, `value` or `null`;
//! - `kv.ready`: nothing.
//!
//! A store is its node's own: two nodes running one harness keep two stores, so a key is a subject
//! of the node that wrote it.

use bohr_sdk_proto::json::Json;

use super::{Ack, Fact, Read};

/// The facts one `kv.*` record from `node`, in its start `start`, holds.
pub(super) fn facts(node: u32, start: u64, name: &str, details: &Json) -> Vec<Fact> {
    match name {
        "kv.begun" => ops(node, start, details).into_iter().map(Fact::Begun).collect(),
        "kv.acked" => ops(node, start, details).into_iter().map(Fact::Acked).collect(),
        "kv.read" => {
            let Some(key) = details.get("key").and_then(Json::as_str) else { return Vec::new() };
            vec![Fact::Read(Read {
                subject: subject(node, key),
                value: details.get("value").and_then(text),
                node,
                start,
                owes: true,
            })]
        }
        "kv.ready" => vec![Fact::Ready { node }],
        _ => Vec::new(),
    }
}

/// The operations of one `kv.begun` or `kv.acked`: one per key, sharing the batch's sequence.
/// The operations of one record. A harness's sequence orders its operations within one start; a
/// later start's are later whatever their numbers, since a harness may count from one again.
fn ops(node: u32, start: u64, details: &Json) -> Vec<Ack> {
    let Some(seq) = details
        .get("seq")
        .and_then(Json::as_int)
        .and_then(|n| u32::try_from(n).ok())
        .map(|seq| start << 32 | u64::from(seq))
    else {
        return Vec::new();
    };
    let batch = (details.get("op").and_then(Json::as_str) == Some("batch")).then_some(seq);
    let removal = details.get("op").and_then(Json::as_str) == Some("delete");
    let at = details.get("at").and_then(|at| {
        Some(format!("{}:{}", at.get("file")?.as_str()?, at.get("line")?.as_int()?))
    });
    let keys = match details.get("keys") {
        Some(Json::Array(keys)) => keys.iter().filter_map(Json::as_str).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let values = match details.get("values") {
        Some(Json::Array(values)) => values.clone(),
        _ => Vec::new(),
    };
    keys.into_iter()
        .enumerate()
        .map(|(i, key)| Ack {
            subject: subject(node, key),
            value: if removal { None } else { values.get(i).and_then(text) },
            batch,
            seq,
            at: at.clone(),
        })
        .collect()
}

/// A key as a subject: the node's store, then the key.
fn subject(node: u32, key: &str) -> String {
    format!("node {node} key {key}")
}

/// A value as text: a string as it is, a number in decimal; `null` is no value.
fn text(value: &Json) -> Option<String> {
    match value {
        Json::Str(s) => Some(s.clone()),
        Json::Int(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bohr_sdk_proto::json;

    use super::*;

    #[test]
    fn a_batch_is_one_operation_per_key_sharing_its_sequence() {
        let details = json::parse(
            r#"{"op":"batch","keys":["a","b"],"values":["1",null],"seq":5,"at":{"file":"s.c","line":9}}"#,
        )
        .expect("json");
        let facts = facts(0, 1, "kv.acked", &details);
        let Fact::Acked(first) = &facts[0] else { panic!("{facts:?}") };
        assert_eq!(
            (first.subject.as_str(), first.value.as_deref(), first.batch),
            ("node 0 key a", Some("1"), Some(1 << 32 | 5))
        );
        assert_eq!(first.at.as_deref(), Some("s.c:9"));
        let Fact::Acked(second) = &facts[1] else { panic!("{facts:?}") };
        assert_eq!(second.value, None);
        let read = facts_of_read();
        assert!(matches!(&read[0], Fact::Read(r) if r.value.is_none() && r.start == 2));
    }

    fn facts_of_read() -> Vec<Fact> {
        facts(0, 2, "kv.read", &json::parse(r#"{"key":"a","value":null}"#).expect("json"))
    }
}
