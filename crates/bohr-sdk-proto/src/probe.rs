//! The record a probe writes for each round of answers, written and read here alone.
//!
//! A round is one SDK event named [`PROBE_EVENT`] whose details are `{"round": [item, ...]}`, each
//! item `{"request": index, "result": value}` or `{"request": index, "unanswered": why}`. The
//! probe writes it, and the world, the report and the command line read it, all through [`Round`].

use std::collections::BTreeMap;

use crate::json::Json;

/// The name of the SDK event a probe reports a round of answers under.
pub const PROBE_EVENT: &str = "bohr_probe";

/// What one request came back with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    /// The result, reduced to the requested fields.
    Result(Json),
    /// Why there is no result: refused, timed out, unreadable, or an error the node returned.
    Unanswered(String),
}

/// One round: each request's answer, by the request's index in the probe's plan, in order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Round(pub Vec<(usize, Answer)>);

impl Round {
    /// The event details this round is written as.
    #[must_use]
    pub fn to_details(&self) -> Json {
        let items = self
            .0
            .iter()
            .map(|(request, answer)| {
                let mut item = BTreeMap::new();
                item.insert("request".to_owned(), Json::Int(*request as i64));
                match answer {
                    Answer::Result(result) => item.insert("result".to_owned(), result.clone()),
                    Answer::Unanswered(why) => {
                        item.insert("unanswered".to_owned(), Json::Str(why.clone()))
                    }
                };
                Json::Object(item)
            })
            .collect();
        let mut details = BTreeMap::new();
        details.insert("round".to_owned(), Json::Array(items));
        Json::Object(details)
    }

    /// The round `details` hold, or `None` where they hold none.
    ///
    /// An item without a request index, or with neither a result nor a reason, is dropped; a
    /// reason that is not text reads as `unanswered`.
    #[must_use]
    pub fn from_details(details: &Json) -> Option<Self> {
        let Some(Json::Array(items)) = details.get("round") else {
            return None;
        };
        let answers = items
            .iter()
            .filter_map(|item| {
                let request = usize::try_from(item.get("request")?.as_int()?).ok()?;
                let answer = match (item.get("result"), item.get("unanswered")) {
                    (Some(result), _) => Answer::Result(result.clone()),
                    (None, Some(why)) => {
                        Answer::Unanswered(why.as_str().unwrap_or("unanswered").to_owned())
                    }
                    (None, None) => return None,
                };
                Some((request, answer))
            })
            .collect();
        Some(Self(answers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A round reads back as it was written, a `null` result included.
    #[test]
    fn a_round_reads_back_as_written() {
        let round = Round(vec![
            (0, Answer::Result(Json::Str("0x1".to_owned()))),
            (1, Answer::Unanswered("refused".to_owned())),
            (9, Answer::Result(Json::Null)),
        ]);
        let text = round.to_details().to_json();
        let read = crate::json::parse(&text).expect("JSON");
        assert_eq!(Round::from_details(&read), Some(round));
        assert_eq!(Round::from_details(&Json::Null), None, "not a round");
    }
}
