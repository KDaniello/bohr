//! A chain's records, as the laws' vocabulary.
//!
//! The load writes what each node answered: `load.committed {tx, node, height, succeeded}` when a
//! transaction is found in a block, `load.head {node, height}` each time a node is asked its head,
//! and, after the fault window and the settle time, `load.final {tx, node, held, head}` for every
//! committed transaction on every node. A transaction committed at a height is owed by every node
//! whose head is at or past it.

use std::collections::BTreeMap;

use bohr_sdk_proto::json::Json;

use super::{Ack, Fact, Read};

/// What the chain's records said so far: the lowest height each transaction was committed at.
#[derive(Debug, Default)]
pub(super) struct Chain {
    committed_at: BTreeMap<u64, i64>,
}

impl Chain {
    /// The facts one `load.*` record holds; `start_of` names a node's current start.
    pub(super) fn facts(
        &mut self,
        name: &str,
        details: &Json,
        start_of: impl Fn(u32) -> u64,
    ) -> Vec<Fact> {
        let int = |key: &str| details.get(key).and_then(Json::as_int);
        let node = int("node").and_then(|n| u32::try_from(n).ok());
        let tx = int("tx").and_then(|n| u64::try_from(n).ok());
        match (name, node, tx) {
            ("load.head", Some(node), _) if int("height").is_some() => vec![Fact::Ready { node }],
            ("load.committed", Some(node), Some(tx)) => {
                let succeeded = details.get("succeeded").and_then(Json::as_bool) == Some(true);
                let Some(height) = int("height").filter(|_| succeeded) else { return Vec::new() };
                let first = !self.committed_at.contains_key(&tx);
                let at = self.committed_at.entry(tx).or_insert(height);
                *at = (*at).min(height);
                let mut facts = vec![Fact::Ready { node }];
                if first {
                    facts.push(Fact::Acked(Ack {
                        subject: subject(tx),
                        value: Some(HELD.to_owned()),
                        batch: None,
                        seq: tx,
                        at: None,
                    }));
                }
                facts
            }
            // A node that did not answer said nothing about what it holds.
            ("load.final", Some(node), Some(tx)) if details.get("held").is_some() => {
                let held = details.get("held").and_then(Json::as_bool) == Some(true);
                let owes = match (int("head"), self.committed_at.get(&tx)) {
                    (Some(head), Some(at)) => head >= *at,
                    _ => false,
                };
                vec![Fact::Read(Read {
                    subject: subject(tx),
                    value: held.then(|| HELD.to_owned()),
                    node,
                    start: start_of(node),
                    owes,
                })]
            }
            _ => Vec::new(),
        }
    }
}

/// What a committed transaction holds, for a law that compares values.
const HELD: &str = "committed";

fn subject(tx: u64) -> String {
    format!("tx {tx}")
}

#[cfg(test)]
mod tests {
    use bohr_sdk_proto::json;

    use super::*;

    #[test]
    fn a_node_past_the_commit_height_owes_the_transaction_and_one_below_does_not() {
        let mut chain = Chain::default();
        let committed =
            json::parse(r#"{"tx":3,"node":0,"height":10,"succeeded":true}"#).expect("json");
        let facts = chain.facts("load.committed", &committed, |_| 1);
        assert!(facts.iter().any(|f| matches!(f, Fact::Acked(a) if a.subject == "tx 3")));
        let past = json::parse(r#"{"tx":3,"node":1,"held":false,"head":12}"#).expect("json");
        let below = json::parse(r#"{"tx":3,"node":2,"held":false,"head":9}"#).expect("json");
        let owes = |details: &Json, chain: &mut Chain| match &chain.facts(
            "load.final",
            details,
            |_| 2,
        )[..]
        {
            [Fact::Read(read)] => read.owes,
            other => panic!("{other:?}"),
        };
        assert!(owes(&past, &mut chain));
        assert!(!owes(&below, &mut chain));
    }
}
