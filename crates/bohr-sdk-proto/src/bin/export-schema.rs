//! Writes the JSON Schema for the record format to standard output.
//!
//! What keeps the schema and the code from drifting: a job regenerates this and fails on a diff,
//! so a field added to a record without a thought for the schema stops a change rather than
//! shipping a document that quietly describes the version before it.
//!
//! Written with the crate's own JSON rather than with a schema library, for the same reason the
//! crate has no dependencies at all: it is linked into a customer's process through the Rust SDK,
//! and a library in that position is a version conflict waiting for them. The document is small
//! and its shape is stable, so hand-writing it costs a page and saves that.

use std::collections::BTreeMap;

use bohr_sdk_proto::{AssertType, SDK_SCHEMA, json::Json};

fn main() {
    println!("{}", schema().to_json());
}

/// The whole document.
fn schema() -> Json {
    object(&[
        ("$schema", string("https://json-schema.org/draft/2020-12/schema")),
        ("$id", string("https://bohr.sh/schema/records.schema.json")),
        ("title", string("Bohr SDK record")),
        (
            "description",
            string(
                "One line of the record channel. Normative text is docs/API.md \u{a7}6.3; this \
                 document is generated from the Rust types that read and write these records.",
            ),
        ),
        ("type", string("object")),
        // A record written by a newer SDK carries keys this version has no name for, and the tool
        // preserves them verbatim rather than refusing the line. A schema that forbade them would
        // describe a stricter format than the one implemented.
        ("additionalProperties", Json::Bool(true)),
        (
            "properties",
            object(&[
                ("bohr", meta()),
                ("antithesis_assert", assert_record()),
                ("antithesis_setup", named_with_details()),
                ("antithesis_event", named_with_details()),
                ("bohr_fault_point", fault_point()),
            ]),
        ),
    ])
}

/// What every record carries about itself.
fn meta() -> Json {
    object(&[
        ("type", string("object")),
        ("required", array(&[string("schema"), string("seq")])),
        (
            "properties",
            object(&[
                (
                    "schema",
                    object(&[
                        ("type", string("integer")),
                        ("const", Json::Int(i64::from(SDK_SCHEMA))),
                    ]),
                ),
                (
                    "seq",
                    object(&[
                        ("type", string("integer")),
                        ("minimum", Json::Int(1)),
                        (
                            "description",
                            string(
                                "Per-process and monotonic. A gap is how the supervisor notices a \
                                 record it never saw.",
                            ),
                        ),
                    ]),
                ),
                ("node", object(&[("type", string("integer")), ("minimum", Json::Int(0))])),
            ]),
        ),
    ])
}

/// A promise the guest makes about itself.
fn assert_record() -> Json {
    object(&[
        ("type", string("object")),
        (
            "required",
            array(&[
                string("hit"),
                string("must_hit"),
                string("assert_type"),
                string("message"),
                string("condition"),
                string("id"),
                string("location"),
            ]),
        ),
        (
            "properties",
            object(&[
                ("hit", object(&[("type", string("boolean"))])),
                ("must_hit", object(&[("type", string("boolean"))])),
                ("assert_type", object(&[("enum", assert_types())])),
                ("display_type", object(&[("type", string("string"))])),
                ("message", object(&[("type", string("string"))])),
                ("condition", object(&[("type", string("boolean"))])),
                ("id", object(&[("type", string("string"))])),
                ("location", location()),
            ]),
        ),
    ])
}

/// Every kind of promise, named as the wire names them.
///
/// Taken from the type rather than written out again: the list is what the code accepts, and a
/// schema that listed anything else would be describing a format nothing implements.
fn assert_types() -> Json {
    Json::Array(
        [
            AssertType::Always,
            AssertType::AlwaysOrUnreachable,
            AssertType::Sometimes,
            AssertType::Reachable,
            AssertType::Unreachable,
        ]
        .into_iter()
        .map(|kind| string(kind.as_str()))
        .collect(),
    )
}

/// Where in the guest's own source something is written.
fn location() -> Json {
    object(&[
        ("type", string("object")),
        ("required", array(&[string("file"), string("function"), string("begin_line")])),
        (
            "properties",
            object(&[
                ("file", object(&[("type", string("string"))])),
                ("function", object(&[("type", string("string"))])),
                ("begin_line", object(&[("type", string("integer"))])),
                ("begin_column", object(&[("type", string("integer"))])),
            ]),
        ),
    ])
}

/// A fault point the guest passed through.
fn fault_point() -> Json {
    object(&[
        ("type", string("object")),
        ("required", array(&[string("name"), string("armed")])),
        (
            "properties",
            object(&[
                ("name", object(&[("type", string("string"))])),
                ("armed", object(&[("type", string("boolean"))])),
            ]),
        ),
    ])
}

/// Setup and event records: a name, and whatever the guest chose to attach to it.
fn named_with_details() -> Json {
    object(&[
        ("type", string("object")),
        (
            "description",
            string(
                "A name the guest chose and whatever it attached. The attachment is the guest's \
                 own data and is what redaction removes; the name is kept.",
            ),
        ),
        ("additionalProperties", Json::Bool(true)),
    ])
}

fn object(fields: &[(&str, Json)]) -> Json {
    let mut map = BTreeMap::new();
    for (key, value) in fields {
        map.insert((*key).to_owned(), value.clone());
    }
    Json::Object(map)
}

fn array(items: &[Json]) -> Json {
    Json::Array(items.to_vec())
}

fn string(text: &str) -> Json {
    Json::Str(text.to_owned())
}
