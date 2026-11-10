//! Removing from a bundle what a customer would not want published.
//!
//! A bundle holds the customer's data: environment, arguments, records. `docs/API.md` §4.6 lists
//! what is removed.
//!
//! A bundle goes in and a bundle comes out, so the redacted bundle still replays.
//!
//! # The event log
//!
//! The event log is unchanged: it records lengths and eight-byte digests, never data.

use crate::{Bundle, SectionId, TraceError};

/// What a redaction took out, for the note it leaves on the bundle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Removed {
    paths: usize,
    records: usize,
    config_lines: usize,
}

/// Rewrites a bundle without what §4.6 says must not travel.
///
/// # Errors
///
/// Reports a bundle whose sections cannot be re-read, which means it was not one, and one whose
/// configuration or recorded binaries this build cannot prove it has cleaned.
pub fn redact(bundle: &Bundle) -> Result<Vec<u8>, TraceError> {
    let mut removed = Removed::default();
    let mut writer = bundle.to_writer();

    if let Some(binaries) = bundle.section(SectionId::Binaries) {
        let (rewritten, count) = hash_paths(binaries)?;
        removed.paths = count;
        writer.section(SectionId::Binaries, rewritten);
    }
    if let Some(records) = bundle.section(SectionId::SdkRecords) {
        let (rewritten, count) = strip_details(records);
        removed.records = count;
        writer.section(SectionId::SdkRecords, rewritten);
    }
    if let Some(config) = bundle.section(SectionId::Config) {
        let (rewritten, count) = drop_argv_and_env(config)?;
        removed.config_lines = count;
        writer.section(SectionId::Config, rewritten);
    }

    // The bundle notes that it is redacted.
    let mut notes = bundle
        .section(SectionId::Notes)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    if !notes.is_empty() && !notes.ends_with('\n') {
        notes.push('\n');
    }
    notes.push_str(&format!(
        "redacted: {} binary paths hashed, {} record payloads removed, {} configuration lines \
         removed\n",
        removed.paths, removed.records, removed.config_lines
    ));
    writer.section(SectionId::Notes, notes.into_bytes());

    // The same run identifier, so it matches its finding.
    Ok(writer.finish(bundle.run_id))
}

/// Replaces each recorded path with a digest of it, keeping the fingerprint that identifies the
/// object itself.
///
/// The fingerprint identifies the binary; the path reveals the customer's layout.
///
/// A `builtin:` name is kept, since replay needs it and it reveals nothing.
///
/// # Errors
///
/// Refuses a section it cannot read or write back.
fn hash_paths(binaries: &[u8]) -> Result<(Vec<u8>, usize), TraceError> {
    let mut records: Vec<crate::BinaryRecord> =
        serde_json::from_slice(binaries).map_err(|e| TraceError::RedactionIncomplete {
            reason: format!("the recorded binaries cannot be read, so their paths cannot go: {e}"),
        })?;
    let mut count = 0;
    for record in &mut records {
        // Arguments are removed too, as `drop_argv_and_env` does in the configuration.
        if !record.args.is_empty() {
            record.args.clear();
            count += 1;
        }
        if !record.path.is_empty() && !record.path.starts_with("builtin:") {
            record.path = format!("blake3:{}", bohr_types::Digest::of(record.path.as_bytes()));
            count += 1;
        }
    }
    let rewritten =
        serde_json::to_vec_pretty(&records).map_err(|e| TraceError::RedactionIncomplete {
            reason: format!("the cleaned list of binaries cannot be written back: {e}"),
        })?;
    Ok((rewritten, count))
}

/// Removes the payloads a guest attached to its own records, keeping what each record is called.
///
/// An unparsable line is dropped, since it may hide a payload.
///
/// Each dropped line leaves a marker, so `EventKind::SdkRecord { index }` keeps pointing at the
/// right record.
fn strip_details(records: &[u8]) -> (Vec<u8>, usize) {
    let mut kept = String::new();
    let mut removed = 0;
    for line in String::from_utf8_lossy(records).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let rewritten = match bohr_sdk_proto::Record::parse_line(line) {
            // The record keeps every writer field, `details` included, so it is cleared here.
            Ok(bohr_sdk_proto::Record::Assert(assertion, meta)) => {
                bohr_sdk_proto::Record::Assert(assertion.without_attachments(), meta).to_line()
            }
            Ok(bohr_sdk_proto::Record::FaultPoint(point, meta)) => {
                bohr_sdk_proto::Record::FaultPoint(point, meta).to_line()
            }
            // Keep the name; drop the attachment.
            Ok(_) | Err(_) => {
                removed += 1;
                REMOVED_RECORD.to_owned()
            }
        };
        kept.push_str(&rewritten);
        // One record per line.
        kept.push('\n');
    }
    (kept.into_bytes(), removed)
}

/// What stands in for a record a redaction took out.
///
/// A line, so record positions stay; it parses as an unknown record.
const REMOVED_RECORD: &str = r#"{"bohr_redacted":{}}"#;

/// Removes what a program was started with: its arguments, and its environment.
///
/// Filtered as text, so the result reads as a diff of what the author wrote.
///
/// The scanner follows multi-line values and `[env]` tables. The result is then parsed, and any
/// surviving forbidden key is `TraceError::RedactionIncomplete`.
///
/// # Errors
///
/// Refuses a configuration that is not readable as TOML, and one where a forbidden key survives
/// the pass.
fn drop_argv_and_env(config: &[u8]) -> Result<(Vec<u8>, usize), TraceError> {
    let text = String::from_utf8_lossy(config).into_owned();
    // An unparsable configuration is refused.
    if toml::from_str::<toml::Value>(&text).is_err() {
        return Err(TraceError::RedactionIncomplete {
            reason: "the configuration is not readable as TOML".to_owned(),
        });
    }

    let mut kept = String::new();
    let mut removed = 0usize;
    // Depth, so a multi-line value goes with its key.
    let mut depth = 0usize;
    let mut dropping = false;
    let mut in_forbidden_table = false;

    for line in text.lines() {
        if depth == 0 {
            let trimmed = line.trim_start();
            if let Some(name) = table_header(trimmed) {
                // A table runs to the next header. `[env]`, `[node.env]` and `[[env]]` are all the
                // environment.
                in_forbidden_table = name.rsplit('.').next().is_some_and(is_forbidden);
                dropping = false;
                if in_forbidden_table {
                    removed += 1;
                    kept.push_str(REMOVED_LINE);
                    continue;
                }
            } else if in_forbidden_table {
                if !trimmed.is_empty() {
                    removed += 1;
                }
                kept.push_str(REMOVED_LINE);
                continue;
            } else if key_of(trimmed).is_some_and(is_forbidden) {
                dropping = true;
                removed += 1;
            }
        }

        depth = depth_after(line, depth);
        if dropping {
            kept.push_str(REMOVED_LINE);
            if depth == 0 {
                dropping = false;
            }
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }

    // The parser confirms nothing forbidden survived.
    let parsed =
        toml::from_str::<toml::Value>(&kept).map_err(|e| TraceError::RedactionIncomplete {
            reason: format!("removing arguments and environment left unreadable TOML: {e}"),
        })?;
    if let Some(surviving) = forbidden_key_in(&parsed) {
        return Err(TraceError::RedactionIncomplete {
            reason: format!("`{surviving}` survived the pass"),
        });
    }

    Ok((kept.into_bytes(), removed))
}

/// What stands in for a configuration line a redaction took out.
const REMOVED_LINE: &str = "# removed by bohr report --redact\n";

/// Whether a key names what a program was started with.
fn is_forbidden(key: &str) -> bool {
    let key = key.trim().trim_matches(['"', '\'']);
    key == "argv" || key == "env"
}

/// The name of the table a line opens, where it opens one.
fn table_header(trimmed: &str) -> Option<&str> {
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    Some(inner.strip_prefix('[').and_then(|i| i.strip_suffix(']')).unwrap_or(inner))
}

/// The key a line assigns to, where it assigns to one.
fn key_of(trimmed: &str) -> Option<&str> {
    let (key, _) = trimmed.split_once('=')?;
    (!key.trim().is_empty()).then(|| key.trim())
}

/// The nesting depth a line leaves behind it, given what it started at.
///
/// Brackets inside strings and comments are ignored.
fn depth_after(line: &str, mut depth: usize) -> usize {
    let mut chars = line.chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('"'), '\\') => {
                // An escape in a basic string.
                let _ = chars.next();
            }
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') => break,
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth = depth.saturating_sub(1),
            (None, _) => {}
        }
    }
    depth
}

/// The first key anywhere in `value` that names what must not travel.
fn forbidden_key_in(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::Table(table) => table.iter().find_map(|(key, held)| {
            if is_forbidden(key) {
                return Some(key.clone());
            }
            forbidden_key_in(held)
        }),
        toml::Value::Array(items) => items.iter().find_map(forbidden_key_in),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `builtin:` name is kept.
    #[test]
    fn a_guest_built_into_the_tool_keeps_its_name() {
        let binaries = serde_json::to_vec(&vec![crate::BinaryRecord {
            path: "builtin:two-tasks".to_owned(),
            blake3: String::new(),
            role: "builtin".to_owned(),
            args: Vec::new(),
        }])
        .expect("json");

        let (rewritten, count) = hash_paths(&binaries).expect("readable");
        assert_eq!(count, 0, "a guest inside the tool was treated as somebody file");
        let back: Vec<crate::BinaryRecord> = serde_json::from_slice(&rewritten).expect("json");
        assert_eq!(back[0].path, "builtin:two-tasks");
    }

    #[test]
    fn a_path_becomes_a_digest_and_the_fingerprint_stays() {
        let binaries = serde_json::to_vec(&vec![crate::BinaryRecord {
            path: "/home/someone/secret-project/target/release/app".to_owned(),
            blake3: "blake3:abc".to_owned(),
            role: "executable".to_owned(),
            args: vec!["--token".to_owned(), "hunter2".to_owned()],
        }])
        .expect("json");

        let (rewritten, count) = hash_paths(&binaries).expect("readable");
        assert_eq!(count, 2, "the path and what the program was started with both go");
        let back: Vec<crate::BinaryRecord> = serde_json::from_slice(&rewritten).expect("json");
        assert!(!back[0].path.contains("secret-project"), "{}", back[0].path);
        assert!(back[0].args.is_empty(), "what the program was started with survived");
        assert_eq!(back[0].blake3, "blake3:abc", "the fingerprint was taken too");
    }

    /// An unreadable binaries section is refused.
    #[test]
    fn binaries_that_cannot_be_read_are_refused_rather_than_passed_through() {
        let outcome = hash_paths(b"{not json at all");
        assert!(matches!(outcome, Err(TraceError::RedactionIncomplete { .. })), "{outcome:?}");
    }

    #[test]
    fn what_a_program_was_started_with_is_removed_and_the_world_is_not() {
        let config = b"[cluster]\nnodes = 5\nargv = [\"/opt/app\", \"--token\", \"hunter2\"]\n\
                       env = { API_KEY = \"secret\" }\n\n[network]\ndrop_probability = \"1/1000\"\n";
        let (rewritten, count) = drop_argv_and_env(config).expect("removable");
        let text = String::from_utf8(rewritten).expect("text");

        assert_eq!(count, 2);
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("API_KEY"), "{text}");
        assert!(text.contains("drop_probability"), "the world was taken too: {text}");
        assert!(text.contains("nodes = 5"), "the shape of the cluster was taken too: {text}");
    }

    /// Multi-line values and tables are removed whole.
    #[test]
    fn a_value_spread_over_several_lines_goes_with_its_key() {
        let config = b"[cluster]\nnodes = 5\nargv = [\n  \"/opt/app\",\n  \"--token\",\n  \
                       \"hunter2\",\n]\nafter = 1\n\n[env]\nAPI_KEY = \"secret\"\n\
                       HOME = \"/home/someone\"\n\n[network]\ndrop_probability = \"1/1000\"\n";
        let (rewritten, count) = drop_argv_and_env(config).expect("removable");
        let text = String::from_utf8(rewritten).expect("text");

        assert!(!text.contains("hunter2"), "a continuation line survived: {text}");
        assert!(!text.contains("API_KEY"), "a table contents survived: {text}");
        assert!(!text.contains("/home/someone"), "a table contents survived: {text}");
        assert!(text.contains("after = 1"), "the key after the array was taken too: {text}");
        assert!(text.contains("drop_probability"), "the table after was taken too: {text}");
        assert!(text.contains("nodes = 5"), "the key before was taken too: {text}");
        assert!(count >= 2, "removals were undercounted: {count}");
    }

    /// A nested environment table is removed.
    #[test]
    fn an_environment_nested_under_a_table_goes_too() {
        let config = b"[node]\nname = \"a\"\n\n[node.env]\nTOKEN = \"secret\"\n";
        let text = String::from_utf8(drop_argv_and_env(config).expect("removable").0).expect("t");
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("name = \"a\""), "{text}");
    }

    /// Brackets inside a string do not change depth.
    #[test]
    fn a_bracket_inside_a_string_does_not_move_the_scanner() {
        let config = b"[cluster]\nargv = [\"a]b\", \"c[d\"]\nnodes = 5\n";
        let text = String::from_utf8(drop_argv_and_env(config).expect("removable").0).expect("t");
        assert!(!text.contains("a]b"), "{text}");
        assert!(text.contains("nodes = 5"), "the key after the array was taken too: {text}");
    }

    /// An unreadable configuration is refused.
    #[test]
    fn a_configuration_that_cannot_be_read_is_refused() {
        let outcome = drop_argv_and_env(b"[cluster\nnodes = ");
        assert!(matches!(outcome, Err(TraceError::RedactionIncomplete { .. })), "{outcome:?}");
    }

    /// An unreadable record line is dropped.
    #[test]
    fn a_record_this_build_cannot_read_is_dropped_rather_than_passed_through() {
        let records = b"{\"something_new\":{\"secret\":\"value\"}}\nnot json at all\n";
        let (rewritten, removed) = strip_details(records);
        let text = String::from_utf8(rewritten).expect("text");
        assert_eq!(removed, 2);
        assert!(!text.contains("secret"), "a payload survived: {text}");
        assert_eq!(text.lines().count(), 2, "the positions of later records moved: {text}");
    }

    /// Two kept records come out as two lines.
    #[test]
    fn records_that_survive_are_still_one_record_to_a_line() {
        let records = b"{\"antithesis_assert\":{\"assert_type\":\"always\",\"condition\":false,\
                        \"message\":\"first\"}}\n\
                        {\"antithesis_assert\":{\"assert_type\":\"always\",\"condition\":false,\
                        \"message\":\"second\"}}\n";
        let (rewritten, removed) = strip_details(records);
        let text = String::from_utf8(rewritten).expect("text");

        assert_eq!(removed, 0);
        assert_eq!(text.lines().count(), 2, "two records came out as {text:?}");
        for line in text.lines() {
            let record = bohr_sdk_proto::Record::parse_line(line).expect("a line we wrote");
            assert!(matches!(record, bohr_sdk_proto::Record::Assert(..)), "{record:?}");
        }
        assert!(text.contains("first") && text.contains("second"), "{text}");
    }

    /// An assertion keeps its identity and loses its attachment.
    #[test]
    fn what_a_guest_attached_to_an_assertion_does_not_survive() {
        let records = b"{\"antithesis_assert\":{\"assert_type\":\"always\",\"condition\":false,\
                        \"message\":\"log is monotonic\",\"details\":{\"token\":\"hunter2\"},\
                        \"location\":{\"file\":\"src/log.rs\",\"class\":\"raft::log\"}}}\n";
        let (rewritten, removed) = strip_details(records);
        let text = String::from_utf8(rewritten).expect("text");

        assert_eq!(removed, 0, "the assertion itself was dropped: {text}");
        assert!(!text.contains("hunter2"), "an attachment survived: {text}");
        assert!(!text.contains("raft::log"), "their location field survived: {text}");
        assert!(text.contains("log is monotonic"), "what names the finding was taken: {text}");
        assert!(text.contains("\"condition\":false"), "the verdict was taken: {text}");
    }
}
