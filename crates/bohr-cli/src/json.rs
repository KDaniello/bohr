//! `--json`: a command's result as one object on standard output.
//!
//! The object is for programs: pipelines, CI steps, agents through `bohr mcp`. It names its
//! schema, `"schema": "bohr.<command>/<n>"`. The version changes when a field is removed, renamed
//! or changes meaning; an added field keeps it.
//!
//! Under `--json` standard output carries only the object. Progress and warnings stay on standard
//! error. A failed command writes a `bohr.error` object and keeps its exit code.
//!
//! The object arrives when the command ends. `bohr mcp` reports a long campaign while it runs.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value};

/// Whether this invocation asked for JSON. Set once, before any command runs.
static ON: AtomicBool = AtomicBool::new(false);

/// Every schema this build writes, with its version. `docs/API.md` §7.12 documents the list.
pub(crate) const SCHEMAS: &[(&str, u32)] = &[
    ("ablate", 1),
    ("check", 1),
    ("confirm", 1),
    ("doctor", 1),
    ("error", 1),
    ("explain", 1),
    ("explore", 1),
    ("findings", 1),
    ("hunt", 1),
    ("init", 1),
    ("note", 1),
    ("reduce", 1),
    ("replay", 1),
    ("report", 1),
    ("run", 1),
    ("selftest", 1),
    ("survey", 1),
    ("sweep", 1),
    ("trace", 1),
    ("version", 1),
];

pub(crate) fn set(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// Whether output is JSON. A command checks this before it prints text.
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The schema name of `command` as this build writes it.
///
/// # Panics
/// Where `command` is not in [`SCHEMAS`]. The first test that writes such an object fails.
pub(crate) fn schema(command: &str) -> String {
    let version = SCHEMAS
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, version)| *version)
        .unwrap_or_else(|| panic!("no schema named {command}"));
    format!("bohr.{command}/{version}")
}

/// `body` as `command`'s object: every field of `body`, and its schema beside them.
pub(crate) fn object(command: &str, body: Value) -> Value {
    let mut out = Map::new();
    out.insert("schema".to_owned(), Value::String(schema(command)));
    match body {
        Value::Object(fields) => out.extend(fields),
        Value::Null => {}
        other => {
            out.insert("value".to_owned(), other);
        }
    }
    Value::Object(out)
}

/// Writes `command`'s object to standard output, on one line.
pub(crate) fn emit(command: &str, body: Value) {
    println!("{}", object(command, body));
}

/// The object a failed command writes, with the `BOHR-` code its message opens with.
pub(crate) fn error(message: &str, exit: u8) -> Value {
    let code = message
        .split(|c: char| c == ':' || c.is_whitespace())
        .next()
        .filter(|word| word.starts_with("BOHR-"))
        .map(str::to_owned);
    object("error", serde_json::json!({ "code": code, "message": message, "exit": exit }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_object_names_its_schema() {
        let value = object("version", serde_json::json!({ "tool_version": "1" }));
        assert_eq!(value["schema"], "bohr.version/1");
        assert_eq!(value["tool_version"], "1");
    }

    #[test]
    fn an_error_carries_the_code_its_message_opens_with() {
        let value = error("BOHR-1002: cannot read x: gone", 2);
        assert_eq!(value["code"], "BOHR-1002");
        assert_eq!(value["exit"], 2);
        assert_eq!(error("no code here", 3)["code"], Value::Null);
    }

    #[test]
    fn every_schema_is_named_once() {
        let mut names: Vec<&str> = SCHEMAS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SCHEMAS.len());
    }
}
