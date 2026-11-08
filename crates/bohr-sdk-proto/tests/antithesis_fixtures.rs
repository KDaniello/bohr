//! Records written by Antithesis's own SDKs must read here.
//!
//! The compatible subset is an external contract we do not own (`docs/ARCHITECTURE.md` §5.14), so
//! it is checked against captured records rather than against our reading of their documentation. A
//! field we do not model is preserved, never reinterpreted.

use bohr_sdk_proto::{AssertType, Record, json::Json};

fn fixtures() -> String {
    let path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/sdk/antithesis/local-output.jsonl");
    std::fs::read_to_string(path).expect("the fixtures are committed")
}

#[test]
fn every_fixture_line_parses() {
    let text = fixtures();
    let mut seen = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let record = Record::parse_line(line).unwrap_or_else(|e| panic!("{line}\n  {e}"));
        assert!(
            !matches!(record, Record::Unknown(..)),
            "a record in their documented shape was not understood: {line}"
        );
        seen += 1;
    }
    assert!(seen >= 5, "the fixture set shrank to {seen} records");
}

/// Their numeric guards are read and judged, never written by us.
#[test]
fn a_numeric_guard_reads_as_a_campaign_claim() {
    let line = fixtures()
        .lines()
        .find(|l| l.contains("always_greater_than"))
        .expect("a numeric guard fixture")
        .to_owned();
    let Record::Assert(assertion, _) = Record::parse_line(&line).expect("parses") else {
        panic!("expected an assertion");
    };
    assert!(matches!(assertion.assert_type, AssertType::NumericGuard(_)));
    assert!(!assertion.assert_type.is_per_run(), "a numeric guard is judged over a campaign");
}

/// A field of theirs we do not model — `details`, `location.class`, their spelling of
/// `display_type` — survives the trip a record actually makes.
///
/// Through `Record` rather than through the parser under it: reading and writing back is what
/// redaction does to every record it keeps, and a field lost between the two is a field the
/// bundle no longer holds. Testing the layer below would prove the parser keeps what the layer
/// above then drops.
#[test]
fn fields_we_do_not_model_survive_being_read_and_written_back() {
    let line = fixtures()
        .lines()
        .find(|l| l.contains("\"details\"") && l.contains("antithesis_assert"))
        .expect("a fixture carrying details")
        .to_owned();

    let Record::Assert(assertion, _) = Record::parse_line(&line).expect("parses") else {
        panic!("expected an assertion");
    };
    assert!(assertion.rest.contains_key("details"), "an attachment was dropped on the way in");
    assert!(assertion.location.rest.contains_key("class"), "their location field was dropped");

    let again = bohr_sdk_proto::json::parse(&Record::parse_line(&line).expect("parses").to_line())
        .expect("what we write is json");
    let written = again.get("antithesis_assert").expect("the record kept its shape");
    assert!(
        written.get("details").and_then(|d| d.get("index")).is_some(),
        "a nested attachment did not come back out"
    );
    assert_eq!(
        written.get("location").and_then(|l| l.get("class")).and_then(Json::as_str),
        Some("raft::log"),
        "their location field did not come back out"
    );
}

/// Their name for a type is theirs. We synthesise `display_type` for the records we write, and a
/// record we merely read back must keep the spelling it arrived with.
#[test]
fn their_spelling_of_a_type_is_not_replaced_with_ours() {
    let line = fixtures()
        .lines()
        .find(|l| l.contains("AlwaysGreaterThan"))
        .expect("a fixture with their display spelling")
        .to_owned();
    let written = Record::parse_line(&line).expect("parses").to_line();
    assert!(written.contains("AlwaysGreaterThan"), "we overwrote their spelling: {written}");
}

/// Their records carry no `bohr` key, and inventing one would put a foreign record into a
/// sequence it was never part of — which the supervisor then reads as records having gone missing.
#[test]
fn a_foreign_record_claims_no_place_in_our_sequence() {
    for line in fixtures().lines().filter(|l| !l.trim().is_empty()) {
        let record = Record::parse_line(line).expect("parses");
        assert!(record.meta().is_none(), "we invented metadata for their record: {line}");
        assert!(!record.to_line().contains("\"bohr\""), "we wrote our namespace into: {line}");
    }
}

/// The schema a job regenerates and diffs. Checked here as well so a malformed one is a failing
/// test rather than a failing pipeline: the difference is a minute against a round trip.
#[test]
fn the_committed_schema_is_readable_and_describes_this_version() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/schema/records.schema.json");
    let text = std::fs::read_to_string(path).expect("the schema is committed");
    let schema = bohr_sdk_proto::json::parse(&text).expect("the schema is json");

    let meta = schema
        .get("properties")
        .and_then(|properties| properties.get("bohr"))
        .and_then(|meta| meta.get("properties"))
        .and_then(|properties| properties.get("schema"))
        .and_then(|version| version.get("const"))
        .expect("the schema states which version it describes");
    assert_eq!(
        meta,
        &bohr_sdk_proto::json::Json::Int(i64::from(bohr_sdk_proto::SDK_SCHEMA)),
        "the committed schema describes another version than this build writes"
    );
}
