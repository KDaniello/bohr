//! A consensus log's records, as the laws' vocabulary.
//!
//! A replica of a consensus library writes what it committed, which term it led and how far it
//! applied (`docs/API.md` §6.4):
//!
//! - `log.committed`: `index`, `entry` (a hash or the entry itself), `term`;
//! - `log.leader`: `term`, written by the node that claims it;
//! - `log.applied`: `index`.
//!
//! A position is an index, its decision the committed entry; a round is a term, its decider the
//! node that claimed it. A committed entry is final: a consensus log never takes one back.

use bohr_sdk_proto::json::Json;

use super::{Decided, Fact};

/// The facts one `log.*` record from `node` holds.
pub(super) fn facts(node: u32, name: &str, details: &Json) -> Vec<Fact> {
    let int =
        |key: &str| details.get(key).and_then(Json::as_int).and_then(|n| u64::try_from(n).ok());
    match name {
        "log.committed" => {
            let (Some(index), Some(entry)) = (int("index"), details.get("entry").and_then(text))
            else {
                return Vec::new();
            };
            vec![Fact::Decided(Decided {
                position: index,
                value: entry,
                state: None,
                node,
                final_: true,
            })]
        }
        "log.leader" => {
            int("term").map(|term| vec![Fact::Claimed { round: term, node }]).unwrap_or_default()
        }
        "log.applied" => int("index")
            .map(|index| vec![Fact::Reached { node, position: index }])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A value as text: a string as it is, a number in decimal.
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
    fn a_commit_is_a_final_decision_and_a_leader_a_claim() {
        let committed = json::parse(r#"{"index":4,"entry":"t2","term":2}"#).expect("json");
        assert!(matches!(&facts(1, "log.committed", &committed)[..],
            [Fact::Decided(d)] if d.position == 4 && d.value == "t2" && d.final_ && d.node == 1));
        let leader = json::parse(r#"{"term":3}"#).expect("json");
        assert!(matches!(
            &facts(2, "log.leader", &leader)[..],
            [Fact::Claimed { round: 3, node: 2 }]
        ));
    }
}
