//! Removing from a bundle what a customer would not want published.
//!
//! A bundle is somebody's program under somebody's configuration, so it holds their data:
//! environment, arguments, whatever a guest read or wrote. `docs/API.md` §4.6 says exactly what is
//! removed, and this is where it happens.
//!
//! **A bundle in and a bundle out, rather than a filter over a report.** The contract promises
//! that a redacted bundle still replays, and a filter that only cleaned up a page could not keep
//! that promise: there would be no redacted file to hand anybody. What circulates is the pair — a
//! bundle somebody can run and a page they can read — and both have to be safe to attach.
//!
//! # What is not touched, and why that is not an oversight
//!
//! The event log is unchanged, because it never held the guest's data in the first place. What a
//! syscall read or wrote is recorded as a length and an eight-byte digest, not as bytes, and an
//! address is an address. That was decided when the event was designed, and the effect is that the
//! timeline — the part a reduction and a report both work from — survives redaction intact.

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

    // The bundle says so about itself, so nobody has to remember which copy is which.
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

    // The same run identifier, because it is the same run. A redacted bundle that renamed itself
    // could not be matched to the finding it came from.
    Ok(writer.finish(bundle.run_id))
}

/// Replaces each recorded path with a digest of it, keeping the fingerprint that identifies the
/// object itself.
///
/// A path is a customer's directory layout and often their employer; the fingerprint is what
/// proves two people are talking about the same binary, and is worth keeping.
///
/// A guest built into the tool is left alone. Its `builtin:` name is not a path, discloses nothing
/// about anybody, and is how a replay knows to run a guest rather than look for a file — hashing
/// it would produce a bundle that no longer replays, which is a worse outcome than the one
/// redaction exists to prevent.
///
/// # Errors
///
/// Refuses a section it cannot read or cannot write back. Passing the bytes through untouched and
/// counting nought would note "0 binary paths hashed" over paths still there -- a redaction
/// reporting success for work it had not done.
fn hash_paths(binaries: &[u8]) -> Result<(Vec<u8>, usize), TraceError> {
    let mut records: Vec<crate::BinaryRecord> =
        serde_json::from_slice(binaries).map_err(|e| TraceError::RedactionIncomplete {
            reason: format!("the recorded binaries cannot be read, so their paths cannot go: {e}"),
        })?;
    let mut count = 0;
    for record in &mut records {
        // **The arguments go with the path.** They are the other half of what a program was
        // started with -- the half `drop_argv_and_env` takes out of the configuration -- and a
        // token passed on a command line is a token whichever section of the bundle carries it.
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
/// A line this build cannot parse is dropped rather than kept: it may carry a payload under a name
/// nothing here knows, and a redaction that passed through what it did not understand would be
/// redacting only what it happened to recognise.
///
/// **Every dropped line leaves a marker behind.** Events on the timeline name a record by its
/// position in this section (`EventKind::SdkRecord { index }`), so closing the gap would renumber
/// every record after it and point each event at its neighbour — silently, in the one artefact
/// whose job is to be believed.
fn strip_details(records: &[u8]) -> (Vec<u8>, usize) {
    let mut kept = String::new();
    let mut removed = 0;
    for line in String::from_utf8_lossy(records).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let rewritten = match bohr_sdk_proto::Record::parse_line(line) {
            // Cleared rather than left behind: the record model carries every field the writer
            // supplied, `details` included, so what a guest attached to an assertion leaves only
            // because this line says so.
            Ok(bohr_sdk_proto::Record::Assert(assertion, meta)) => {
                bohr_sdk_proto::Record::Assert(assertion.without_attachments(), meta).to_line()
            }
            Ok(bohr_sdk_proto::Record::FaultPoint(point, meta)) => {
                bohr_sdk_proto::Record::FaultPoint(point, meta).to_line()
            }
            // Setup and event records are a name and whatever the guest chose to attach. The name
            // is what a report shows; the attachment is the customer's.
            Ok(_) | Err(_) => {
                removed += 1;
                REMOVED_RECORD.to_owned()
            }
        };
        kept.push_str(&rewritten);
        // The terminator the record format leaves to whoever writes the stream. Without it two
        // kept records become one line that nothing can read back, and every reader here splits on
        // lines — so a redacted finding would show no assertions at all.
        kept.push('\n');
    }
    (kept.into_bytes(), removed)
}

/// What stands in for a record a redaction took out.
///
/// A line rather than nothing, so the positions events refer to keep meaning what they meant. It
/// parses as a record this build does not model, which is exactly what it is.
const REMOVED_RECORD: &str = r#"{"bohr_redacted":{}}"#;

/// Removes what a program was started with: its arguments, and its environment.
///
/// Filtered rather than reparsed and re-emitted: the configuration a bundle carries is the text
/// somebody wrote, and a replay reads exactly those bytes back. Removing lines can be read off a
/// diff; re-serialising cannot, and would have to be trusted to have changed nothing else.
///
/// **What it cannot prove it removed, it refuses.** Filtering by line alone would let a value
/// spread over several lines lose only its first one -- `argv = [` keeps every argument under it --
/// and would match nothing in an `[env]` table, which is not a key. So the scanner below
/// understands a value's extent, and the result is then parsed and asked whether anything it was
/// meant to remove survived.
/// whether anything it was meant to remove survived. A redaction that cannot answer that question
/// is not a redaction (`TraceError::RedactionIncomplete`).
///
/// # Errors
///
/// Refuses a configuration that is not readable as TOML, and one where a forbidden key survives
/// the pass.
fn drop_argv_and_env(config: &[u8]) -> Result<(Vec<u8>, usize), TraceError> {
    let text = String::from_utf8_lossy(config).into_owned();
    // Read before it is edited, so a configuration nobody can parse is refused rather than
    // filtered by guesswork and handed on.
    if toml::from_str::<toml::Value>(&text).is_err() {
        return Err(TraceError::RedactionIncomplete {
            reason: "the configuration is not readable as TOML".to_owned(),
        });
    }

    let mut kept = String::new();
    let mut removed = 0usize;
    // Depth of the value being read, so the lines a multi-line array or inline table occupies go
    // with the key that opened it.
    let mut depth = 0usize;
    let mut dropping = false;
    let mut in_forbidden_table = false;

    for line in text.lines() {
        if depth == 0 {
            let trimmed = line.trim_start();
            if let Some(name) = table_header(trimmed) {
                // A table stands until the next header, so what to do with its lines is decided
                // once, here. `[env]`, `[node.env]` and `[[env]]` all name the environment.
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

    // The proof. The scanner above understands the shapes it has been shown; this asks the parser
    // whether anything it was meant to take is still there, so a shape nobody anticipated is a
    // refusal rather than a leak.
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
/// Brackets and braces inside a string or a comment are text rather than structure, so the walk
/// tracks both. Without it a path containing a bracket, or a comment mentioning one, would move
/// the depth and take unrelated lines out with the value being removed.
fn depth_after(line: &str, mut depth: usize) -> usize {
    let mut chars = line.chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('"'), '\\') => {
                // An escape inside a basic string: whatever follows is text, quote included.
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

    /// A name that is not a path is not somebody directory layout, and it is what a replay
    /// reconstructs the guest from.
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

    /// A list of binaries this build cannot read is a list whose paths it cannot remove. Passing
    /// the bytes through and reporting "0 binary paths hashed" is a redaction claiming to have
    /// done work it did not do.
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

    /// The shapes the line filter missed. A value may occupy as many lines as it likes, and a
    /// table is not a key at all, so the token stayed in the bundle while the note on it said the
    /// arguments had been removed.
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

    /// A nested table names the environment as surely as a top-level one does.
    #[test]
    fn an_environment_nested_under_a_table_goes_too() {
        let config = b"[node]\nname = \"a\"\n\n[node.env]\nTOKEN = \"secret\"\n";
        let text = String::from_utf8(drop_argv_and_env(config).expect("removable").0).expect("t");
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("name = \"a\""), "{text}");
    }

    /// Brackets inside a string are text. Treating them as structure would take the lines after
    /// them out of an unrelated part of somebody configuration.
    #[test]
    fn a_bracket_inside_a_string_does_not_move_the_scanner() {
        let config = b"[cluster]\nargv = [\"a]b\", \"c[d\"]\nnodes = 5\n";
        let text = String::from_utf8(drop_argv_and_env(config).expect("removable").0).expect("t");
        assert!(!text.contains("a]b"), "{text}");
        assert!(text.contains("nodes = 5"), "the key after the array was taken too: {text}");
    }

    /// The proof at the end of the pass: a configuration this build cannot read is one whose
    /// secrets it cannot find, and it must say so rather than hand the file on.
    #[test]
    fn a_configuration_that_cannot_be_read_is_refused() {
        let outcome = drop_argv_and_env(b"[cluster\nnodes = ");
        assert!(matches!(outcome, Err(TraceError::RedactionIncomplete { .. })), "{outcome:?}");
    }

    /// A line this build cannot read might carry a payload under a name it does not know, so it
    /// goes. Keeping it would mean redacting only what happened to be recognised.
    #[test]
    fn a_record_this_build_cannot_read_is_dropped_rather_than_passed_through() {
        let records = b"{\"something_new\":{\"secret\":\"value\"}}\nnot json at all\n";
        let (rewritten, removed) = strip_details(records);
        let text = String::from_utf8(rewritten).expect("text");
        assert_eq!(removed, 2);
        assert!(!text.contains("secret"), "a payload survived: {text}");
        assert_eq!(text.lines().count(), 2, "the positions of later records moved: {text}");
    }

    /// Two records kept, two lines out. Written without a terminator they became one line that
    /// nothing could read back, so a redacted finding showed no assertions at all -- and the only
    /// test of this path asserted the output was empty, which it was.
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

    /// The record model carries every field its writer supplied, so an assertion arrives here with
    /// the guest's attachment intact. What identifies the finding stays; what the guest hung on it
    /// is the customer's and goes.
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
