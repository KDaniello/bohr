//! The record format an SDK writes and the supervisor reads.
//!
//! Both halves use the same types. The compatible subset is Antithesis's local-output format; our
//! fields live under the `bohr` key (`docs/ARCHITECTURE.md` §5.14).
//!
//! Only `std`, since SDKs are linked into other people's builds. Normative: `docs/API.md` §6.3.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod json;

use std::collections::BTreeMap;

use json::Json;

/// Version of the record schema. Additive: unknown fields inside `bohr` are ignored.
pub const SDK_SCHEMA: u16 = 1;

/// What an assertion claims.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssertType {
    /// Must hold every time it is reached.
    Always,
    /// Must hold every time it is reached, and never being reached is acceptable.
    AlwaysOrUnreachable,
    /// Must hold at least once across a campaign.
    Sometimes,
    /// Must be reached at least once across a campaign.
    Reachable,
    /// Must never be reached.
    Unreachable,
    /// A numeric guard from the Antithesis surface, read but never written (`docs/API.md` §6.4).
    NumericGuard(&'static str),
}

impl AssertType {
    /// The name this type carries in a record.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::AlwaysOrUnreachable => "always_or_unreachable",
            Self::Sometimes => "sometimes",
            Self::Reachable => "reachable",
            Self::Unreachable => "unreachable",
            Self::NumericGuard(name) => name,
        }
    }

    /// The type of that name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "always" => Self::Always,
            "always_or_unreachable" => Self::AlwaysOrUnreachable,
            "sometimes" => Self::Sometimes,
            "reachable" => Self::Reachable,
            "unreachable" => Self::Unreachable,
            "always_greater_than" => Self::NumericGuard("always_greater_than"),
            "always_less_than" => Self::NumericGuard("always_less_than"),
            "sometimes_greater_than" => Self::NumericGuard("sometimes_greater_than"),
            "sometimes_less_than" => Self::NumericGuard("sometimes_less_than"),
            _ => return None,
        })
    }

    /// Whether a false condition is a finding as soon as it happens.
    ///
    /// The rest are campaign-scoped: one run cannot show that something never happens.
    #[must_use]
    pub const fn is_per_run(self) -> bool {
        matches!(self, Self::Always | Self::AlwaysOrUnreachable | Self::Unreachable)
    }
}

/// Where in the guest's source an assertion lives.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Location {
    /// Source file.
    pub file: String,
    /// Function the assertion is in.
    pub function: String,
    /// Line it is on.
    pub begin_line: u32,
    /// Column it starts at.
    pub begin_column: u32,
    /// Everything else the writer put here, kept as it arrived.
    ///
    /// Such as their `class`; see [`AssertRecord::rest`].
    pub rest: BTreeMap<String, Json>,
}

/// What every record carries about the run that produced it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BohrMeta {
    /// Schema version this record was written under.
    pub schema: u16,
    /// Sequence number, per node and monotonic. A gap means records escaped interception.
    pub seq: u64,
    /// The node that wrote it.
    pub node: u32,
}

/// An assertion the guest evaluated.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AssertRecord {
    /// Whether the assertion was reached.
    pub hit: bool,
    /// Whether reaching it at all is required.
    pub must_hit: bool,
    /// What it claims.
    pub assert_type: AssertType,
    /// What the guest called it.
    pub message: String,
    /// Whether the claim held.
    pub condition: bool,
    /// Stable identifier, usually the expression as written.
    pub id: String,
    /// Where it lives.
    pub location: Location,
    /// Every other key the writer put in the record, kept as it arrived.
    ///
    /// Such as `details` and guard operands, so a record reads and writes back unchanged. Never
    /// interpreted (`docs/ARCHITECTURE.md` §5.14); redaction clears it explicitly.
    pub rest: BTreeMap<String, Json>,
}

impl AssertRecord {
    /// Drops everything the guest attached, keeping what the record *is*.
    ///
    /// For redaction: name, type and verdict identify the finding.
    #[must_use]
    pub fn without_attachments(mut self) -> Self {
        self.rest.clear();
        self.location.rest.clear();
        self
    }
}

/// A fault point the guest passed through, and whether it was armed for this run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FaultPointRecord {
    /// What the guest called it.
    pub name: String,
    /// Whether this run armed it.
    pub armed: bool,
}

/// One line of the record stream.
///
/// Run metadata is an [`Option`]: Antithesis's SDKs write no `bohr` key, and none is invented.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Record {
    /// An assertion.
    Assert(AssertRecord, Option<BohrMeta>),
    /// The guest finished setting itself up.
    SetupComplete(Json, Option<BohrMeta>),
    /// A fault point.
    FaultPoint(FaultPointRecord, Option<BohrMeta>),
    /// Something the guest wanted on the timeline.
    Event(String, Json, Option<BohrMeta>),
    /// A record this build does not understand, kept verbatim.
    ///
    /// Kept so newer SDKs' records survive. Its metadata is still read, so its sequence number
    /// leaves no gap.
    Unknown(String, Option<BohrMeta>),
}

impl Record {
    /// What this record says about the run, where it says anything.
    #[must_use]
    pub const fn meta(&self) -> Option<BohrMeta> {
        match self {
            Self::Assert(_, m) | Self::SetupComplete(_, m) | Self::FaultPoint(_, m) => *m,
            Self::Event(_, _, m) | Self::Unknown(_, m) => *m,
        }
    }

    /// The line this record is written as, without its terminator.
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Self::Unknown(raw, _) => raw.clone(),
            Self::Assert(a, meta) => {
                let mut assert = BTreeMap::new();
                assert.insert("hit".to_owned(), Json::Bool(a.hit));
                assert.insert("must_hit".to_owned(), Json::Bool(a.must_hit));
                assert
                    .insert("assert_type".to_owned(), Json::Str(a.assert_type.as_str().to_owned()));
                assert.insert(
                    "display_type".to_owned(),
                    Json::Str(display_type(a.assert_type).to_owned()),
                );
                assert.insert("message".to_owned(), Json::Str(a.message.clone()));
                assert.insert("condition".to_owned(), Json::Bool(a.condition));
                assert.insert("id".to_owned(), Json::Str(a.id.clone()));
                assert.insert("location".to_owned(), location_json(&a.location));
                // Last, so the writer's keys, such as their `display_type`, win.
                assert.extend(a.rest.clone());
                wrap("antithesis_assert", Json::Object(assert), *meta)
            }
            Self::SetupComplete(details, meta) => {
                let mut setup = BTreeMap::new();
                setup.insert("status".to_owned(), Json::Str("complete".to_owned()));
                setup.insert("details".to_owned(), details.clone());
                wrap("antithesis_setup", Json::Object(setup), *meta)
            }
            Self::FaultPoint(point, meta) => {
                let mut body = BTreeMap::new();
                body.insert("name".to_owned(), Json::Str(point.name.clone()));
                body.insert("armed".to_owned(), Json::Bool(point.armed));
                wrap("bohr_fault_point", Json::Object(body), *meta)
            }
            Self::Event(name, details, meta) => {
                let mut body = BTreeMap::new();
                body.insert(name.clone(), details.clone());
                wrap("antithesis_event", Json::Object(body), *meta)
            }
        }
    }

    /// Reads one line.
    ///
    /// A valid line this build does not understand becomes [`Record::Unknown`]. Only unreadable
    /// lines are refused: not JSON, or `bohr` metadata out of range.
    ///
    /// # Errors
    ///
    /// Reports a line that is not JSON, and one whose `bohr` metadata is out of range.
    pub fn parse_line(line: &str) -> Result<Self, String> {
        let value = json::parse(line)?;
        let meta = parse_meta(&value)?;
        let unknown = || Ok(Self::Unknown(line.to_owned(), meta));

        if let Some(a) = value.get("antithesis_assert") {
            // An unknown type or a missing verdict leaves the line unknown; a verdict is never
            // guessed. A missing `hit` defaults, since nothing judges by it.
            let (Some(kind), Some(condition)) = (
                a.get("assert_type").and_then(Json::as_str).and_then(AssertType::parse),
                a.get("condition").and_then(Json::as_bool),
            ) else {
                return unknown();
            };
            return Ok(Self::Assert(
                AssertRecord {
                    hit: a.get("hit").and_then(Json::as_bool).unwrap_or(true),
                    must_hit: a.get("must_hit").and_then(Json::as_bool).unwrap_or(true),
                    assert_type: kind,
                    message: a.get("message").and_then(Json::as_str).unwrap_or_default().to_owned(),
                    condition,
                    id: a.get("id").and_then(Json::as_str).unwrap_or_default().to_owned(),
                    location: location_from(a.get("location")),
                    rest: rest_of(a, ASSERT_FIELDS),
                },
                meta,
            ));
        }
        if let Some(setup) = value.get("antithesis_setup") {
            let details = setup.get("details").cloned().unwrap_or(Json::Null);
            return Ok(Self::SetupComplete(details, meta));
        }
        if let Some(point) = value.get("bohr_fault_point") {
            return Ok(Self::FaultPoint(
                FaultPointRecord {
                    name: point.get("name").and_then(Json::as_str).unwrap_or_default().to_owned(),
                    armed: point.get("armed").and_then(Json::as_bool).unwrap_or(false),
                },
                meta,
            ));
        }
        if let Some(Json::Object(body)) = value.get("antithesis_event") {
            // One key is an event; several are unknown.
            if let [(name, details)] = body.iter().collect::<Vec<_>>()[..] {
                return Ok(Self::Event(name.clone(), details.clone(), meta));
            }
        }
        unknown()
    }
}

/// Keys of an assertion this build models. Everything else is carried in [`AssertRecord::rest`].
const ASSERT_FIELDS: &[&str] = &[
    "hit",
    "must_hit",
    "assert_type",
    // Derived from `assert_type` when written.
    "display_type",
    "message",
    "condition",
    "id",
    "location",
];

/// Keys of a location this build models.
const LOCATION_FIELDS: &[&str] = &["file", "function", "begin_line", "begin_column"];

/// The keys of `value` that are not in `modelled`, cloned as they arrived.
fn rest_of(value: &Json, modelled: &[&str]) -> BTreeMap<String, Json> {
    let Json::Object(map) = value else {
        return BTreeMap::new();
    };
    map.iter()
        .filter(|(key, _)| !modelled.contains(&key.as_str()))
        .map(|(key, held)| (key.clone(), held.clone()))
        .collect()
}

/// Reads our own namespace, where the record carries it.
///
/// Absent stays absent. A field out of range, such as `seq: -1`, makes the line unreadable.
fn parse_meta(value: &Json) -> Result<Option<BohrMeta>, String> {
    let Some(bohr) = value.get("bohr") else {
        return Ok(None);
    };
    let field = |name: &str, ceiling: u64| -> Result<u64, String> {
        match bohr.get(name) {
            None => Ok(0),
            Some(Json::Int(held)) => u64::try_from(*held)
                .ok()
                .filter(|held| *held <= ceiling)
                .ok_or_else(|| format!("bohr.{name} is {held}, which it cannot be")),
            Some(_) => Err(format!("bohr.{name} is not a number")),
        }
    };
    Ok(Some(BohrMeta {
        // A missing version means the version before the key existed.
        schema: field("schema", u64::from(u16::MAX))? as u16,
        seq: field("seq", u64::MAX)?,
        node: field("node", u64::from(u32::MAX))? as u32,
    }))
}

/// The name Antithesis displays a type under.
///
/// Derived from the type, matching Antithesis's spelling, such as `AlwaysGreaterThan`.
const fn display_type(kind: AssertType) -> &'static str {
    match kind {
        AssertType::Always => "Always",
        AssertType::AlwaysOrUnreachable => "AlwaysOrUnreachable",
        AssertType::Sometimes => "Sometimes",
        AssertType::Reachable => "Reachable",
        AssertType::Unreachable => "Unreachable",
        AssertType::NumericGuard(name) => match name.as_bytes() {
            b"always_greater_than" => "AlwaysGreaterThan",
            b"always_less_than" => "AlwaysLessThan",
            b"sometimes_greater_than" => "SometimesGreaterThan",
            b"sometimes_less_than" => "SometimesLessThan",
            // Unreachable for parsed guards; total, since this is presentation.
            _ => name,
        },
    }
}

fn location_json(loc: &Location) -> Json {
    let mut map = BTreeMap::new();
    map.insert("file".to_owned(), Json::Str(loc.file.clone()));
    map.insert("function".to_owned(), Json::Str(loc.function.clone()));
    map.insert("begin_line".to_owned(), Json::Int(i64::from(loc.begin_line)));
    map.insert("begin_column".to_owned(), Json::Int(i64::from(loc.begin_column)));
    map.extend(loc.rest.clone());
    Json::Object(map)
}

fn location_from(value: Option<&Json>) -> Location {
    let Some(v) = value else {
        return Location::default();
    };
    Location {
        file: v.get("file").and_then(Json::as_str).unwrap_or_default().to_owned(),
        function: v.get("function").and_then(Json::as_str).unwrap_or_default().to_owned(),
        begin_line: line_number(v.get("begin_line")),
        begin_column: line_number(v.get("begin_column")),
        rest: rest_of(v, LOCATION_FIELDS),
    }
}

/// A source position, clamped into the range a position can occupy.
///
/// Clamped, since `as u32` would wrap a huge line number to a wrong small one.
fn line_number(value: Option<&Json>) -> u32 {
    value.and_then(Json::as_int).map_or(0, |held| u32::try_from(held).unwrap_or(u32::MAX))
}

fn wrap(key: &str, body: Json, meta: Option<BohrMeta>) -> String {
    let mut top = BTreeMap::new();
    top.insert(key.to_owned(), body);
    // Written only where the record has it.
    if let Some(meta) = meta {
        let mut bohr = BTreeMap::new();
        bohr.insert("schema".to_owned(), Json::Int(i64::from(meta.schema)));
        bohr.insert("seq".to_owned(), Json::Int(i64::try_from(meta.seq).unwrap_or(i64::MAX)));
        bohr.insert("node".to_owned(), Json::Int(i64::from(meta.node)));
        top.insert("bohr".to_owned(), Json::Object(bohr));
    }
    Json::Object(top).to_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> BohrMeta {
        BohrMeta { schema: SDK_SCHEMA, seq: 8814, node: 3 }
    }

    #[test]
    fn an_assertion_round_trips_through_a_line() {
        let record = Record::Assert(
            AssertRecord {
                hit: true,
                must_hit: true,
                assert_type: AssertType::Always,
                message: "log is monotonic".to_owned(),
                condition: false,
                id: "log is monotonic".to_owned(),
                location: Location {
                    file: "src/log.rs".to_owned(),
                    function: "append".to_owned(),
                    begin_line: 220,
                    begin_column: 9,
                    rest: BTreeMap::new(),
                },
                rest: BTreeMap::new(),
            },
            Some(meta()),
        );
        let line = record.to_line();
        assert_eq!(Record::parse_line(&line).expect("parses"), record);
    }

    #[test]
    fn a_record_carries_the_shape_antithesis_reads() {
        let line = Record::FaultPoint(
            FaultPointRecord { name: "before_commit_ack".to_owned(), armed: true },
            Some(meta()),
        )
        .to_line();
        assert!(line.contains("\"bohr_fault_point\""), "{line}");
        assert!(line.contains("\"schema\":1"), "{line}");
        assert!(line.contains("\"node\":3"), "{line}");
    }

    #[test]
    fn a_numeric_guard_is_parsed_though_we_never_write_one() {
        let line = "{\"antithesis_assert\":{\"assert_type\":\"always_greater_than\",\
                    \"condition\":true,\"message\":\"latency\"}}";
        let record = Record::parse_line(line).expect("parses");
        let Record::Assert(a, _) = record else {
            panic!("expected an assertion");
        };
        assert_eq!(a.assert_type, AssertType::NumericGuard("always_greater_than"));
        assert!(!a.assert_type.is_per_run(), "a numeric guard is judged over a campaign");
    }

    #[test]
    fn a_record_this_build_does_not_know_is_kept_verbatim() {
        let line =
            "{\"antithesis_something_new\":{\"x\":1},\"bohr\":{\"schema\":9,\"seq\":1,\"node\":0}}";
        let record = Record::parse_line(line).expect("parses");
        assert_eq!(
            record,
            Record::Unknown(line.to_owned(), Some(BohrMeta { schema: 9, seq: 1, node: 0 }))
        );
        assert_eq!(record.to_line(), line, "an unknown record is written back as it arrived");
    }

    /// An unknown record keeps its sequence number, so the gap check holds.
    #[test]
    fn a_record_this_build_does_not_know_still_holds_its_place_in_the_sequence() {
        let line =
            "{\"antithesis_something_new\":{\"x\":1},\"bohr\":{\"schema\":9,\"seq\":6,\"node\":2}}";
        let meta = Record::parse_line(line).expect("parses").meta().expect("its place is recorded");
        assert_eq!(meta.seq, 6);
        assert_eq!(meta.node, 2);
    }

    /// An unknown assertion type is kept as unknown.
    #[test]
    fn an_assertion_type_this_build_never_heard_of_is_kept_rather_than_refused() {
        let line = "{\"antithesis_assert\":{\"assert_type\":\"always_within\",\"condition\":true}}";
        let record = Record::parse_line(line).expect("an unfamiliar type is not a malformed line");
        assert!(matches!(record, Record::Unknown(..)), "{record:?}");
        assert_eq!(record.to_line(), line, "and it is written back as it arrived");
    }

    /// A record without a verdict is never read as a pass.
    #[test]
    fn an_assertion_with_no_verdict_is_not_read_as_a_passing_one() {
        let line = "{\"antithesis_assert\":{\"assert_type\":\"always\",\"message\":\"m\"}}";
        let record = Record::parse_line(line).expect("parses");
        assert!(matches!(record, Record::Unknown(..)), "{record:?}");
    }

    /// An event record with two keys is unknown.
    #[test]
    fn an_event_carrying_several_names_is_kept_whole_rather_than_halved() {
        let one = "{\"antithesis_event\":{\"elected\":{\"term\":7}}}";
        assert!(matches!(Record::parse_line(one).expect("parses"), Record::Event(..)));

        let two = "{\"antithesis_event\":{\"a\":1,\"b\":2}}";
        let record = Record::parse_line(two).expect("parses");
        assert!(matches!(record, Record::Unknown(..)), "{record:?}");
        assert_eq!(record.to_line(), two, "and both names survive");
    }

    /// Out-of-range metadata makes the line unreadable.
    #[test]
    fn metadata_outside_what_it_can_hold_is_refused() {
        assert!(Record::parse_line("{\"bohr\":{\"seq\":-1}}").is_err());
        assert!(Record::parse_line("{\"bohr\":{\"node\":4294967296}}").is_err());
        assert!(Record::parse_line("{\"bohr\":{\"schema\":65536}}").is_err());
        assert!(Record::parse_line("{\"bohr\":{\"seq\":\"5\"}}").is_err());
    }

    /// Antithesis records get no `bohr` key.
    #[test]
    fn a_record_without_our_namespace_is_given_none() {
        let line = "{\"antithesis_assert\":{\"assert_type\":\"always\",\"condition\":true}}";
        let record = Record::parse_line(line).expect("parses");
        assert!(record.meta().is_none());
        assert!(!record.to_line().contains("bohr"), "{}", record.to_line());
    }

    #[test]
    fn a_malformed_line_is_refused_rather_than_half_read() {
        assert!(Record::parse_line("{\"antithesis_assert\":").is_err());
        assert!(Record::parse_line("not json at all").is_err());
    }

    #[test]
    fn only_per_run_types_can_fail_a_single_run() {
        assert!(AssertType::Always.is_per_run());
        assert!(AssertType::Unreachable.is_per_run());
        assert!(!AssertType::Sometimes.is_per_run(), "one run cannot show something never happens");
        assert!(!AssertType::Reachable.is_per_run());
    }
}
