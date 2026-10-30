//! The JSON this project writes and reads, and nothing more.
//!
//! Hand-rolled because an SDK carries no third-party dependency (`docs/ARCHITECTURE.md` S3): a
//! customer links our SDK into their process, and a serialisation library in that position is a
//! version conflict waiting for them. The supervisor parses with the same code, so the two halves
//! of the contract cannot drift apart.
//!
//! Numbers are integers only. A record that fed a decision back into a run would have to be
//! integral anyway (`docs/API.md` §1 rule 2), and a float that round-trips differently between two
//! libc versions is precisely the class of bug this tool exists to catch.

use std::collections::BTreeMap;

/// Deepest nesting a parser will follow.
///
/// A bound rather than recursion to exhaustion: the parser reads records written by somebody
/// else's build, and a deeply nested document is a stack overflow rather than an error message.
const MAX_DEPTH: usize = 32;

/// A JSON value, as far as records need one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer. JSON's number, restricted to what a decision may depend on.
    Int(i64),
    /// A string.
    Str(String),
    /// An array.
    Array(Vec<Json>),
    /// An object. Ordered, so one value writes one document.
    Object(BTreeMap<String, Json>),
}

impl Json {
    /// The value at a key, where this is an object that has one.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(map) => map.get(key),
            _ => None,
        }
    }

    /// This value as a string, where it is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// This value as an integer, where it is one.
    #[must_use]
    pub const fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// This value as a boolean, where it is one.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Writes this value as JSON.
    pub fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Int(i) => out.push_str(&i.to_string()),
            Self::Str(s) => write_string(s, out),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(map) => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    /// This value as a JSON string.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
}

/// Writes a JSON string, escaping what the grammar requires.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Everything below a space has to be escaped; \u is the only form that works for
            // all of them, and these are rare enough that a general path costs nothing.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parses one JSON document.
///
/// Trailing content is an error rather than ignored: a record is one line, and text after the
/// value is a line that means something other than what it appears to.
pub fn parse(text: &str) -> Result<Json, String> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let value = parse_value(bytes, &mut at, 0)?;
    skip_space(bytes, &mut at);
    if at != bytes.len() {
        return Err(format!("trailing content at byte {at}"));
    }
    Ok(value)
}

fn parse_value(b: &[u8], at: &mut usize, depth: usize) -> Result<Json, String> {
    if depth > MAX_DEPTH {
        return Err("nested deeper than this parser follows".to_owned());
    }
    skip_space(b, at);
    match b.get(*at) {
        None => Err("document ended where a value was expected".to_owned()),
        Some(b'n') => literal(b, at, "null", Json::Null),
        Some(b't') => literal(b, at, "true", Json::Bool(true)),
        Some(b'f') => literal(b, at, "false", Json::Bool(false)),
        Some(b'"') => parse_string(b, at).map(Json::Str),
        Some(b'[') => {
            *at += 1;
            let mut items = Vec::new();
            skip_space(b, at);
            if b.get(*at) == Some(&b']') {
                *at += 1;
                return Ok(Json::Array(items));
            }
            loop {
                items.push(parse_value(b, at, depth + 1)?);
                skip_space(b, at);
                match b.get(*at) {
                    Some(b',') => *at += 1,
                    Some(b']') => {
                        *at += 1;
                        return Ok(Json::Array(items));
                    }
                    _ => return Err(format!("expected , or ] at byte {at}")),
                }
            }
        }
        Some(b'{') => {
            *at += 1;
            let mut map = BTreeMap::new();
            skip_space(b, at);
            if b.get(*at) == Some(&b'}') {
                *at += 1;
                return Ok(Json::Object(map));
            }
            loop {
                skip_space(b, at);
                let key = parse_string(b, at)?;
                skip_space(b, at);
                if b.get(*at) != Some(&b':') {
                    return Err(format!("expected : at byte {at}"));
                }
                *at += 1;
                map.insert(key, parse_value(b, at, depth + 1)?);
                skip_space(b, at);
                match b.get(*at) {
                    Some(b',') => *at += 1,
                    Some(b'}') => {
                        *at += 1;
                        return Ok(Json::Object(map));
                    }
                    _ => return Err(format!("expected , or }} at byte {at}")),
                }
            }
        }
        Some(_) => parse_int(b, at),
    }
}

fn literal(b: &[u8], at: &mut usize, word: &str, value: Json) -> Result<Json, String> {
    if b[*at..].starts_with(word.as_bytes()) {
        *at += word.len();
        return Ok(value);
    }
    Err(format!("not a value at byte {at}"))
}

fn parse_string(b: &[u8], at: &mut usize) -> Result<String, String> {
    if b.get(*at) != Some(&b'"') {
        return Err(format!("expected a string at byte {at}"));
    }
    *at += 1;
    let mut out = String::new();
    loop {
        let Some(&c) = b.get(*at) else {
            return Err("string never ended".to_owned());
        };
        *at += 1;
        match c {
            b'"' => return Ok(out),
            b'\\' => {
                let Some(&esc) = b.get(*at) else {
                    return Err("escape never ended".to_owned());
                };
                *at += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => out.push(parse_escaped_char(b, at)?),
                    _ => return Err(format!("unknown escape at byte {at}")),
                }
            }
            _ => {
                // Multi-byte characters arrive as their own bytes; the slice is valid UTF-8
                // because it came from a `&str`.
                let start = *at - 1;
                let len = utf8_len(c);
                let end = (start + len).min(b.len());
                match std::str::from_utf8(&b[start..end]) {
                    Ok(s) => out.push_str(s),
                    Err(_) => return Err(format!("invalid text at byte {start}")),
                }
                *at = end;
            }
        }
    }
}

/// Reads what follows a `\u`, joining a surrogate pair into the one character it stands for.
///
/// **A pair is the only way JSON can escape a character outside the basic plane**, and every
/// serialiser that escapes non-ASCII emits them — Python's `json.dumps` does it by default, which
/// is what every Python guest writes its records with. Decoding the two halves separately turns
/// one emoji into two replacement characters, and `to_line` then writes the damage back into the
/// bundle as though the guest had produced it.
///
/// A half with no partner is not a character in any encoding; it becomes the replacement
/// character, which keeps a malformed record readable rather than refusing the line.
fn parse_escaped_char(b: &[u8], at: &mut usize) -> Result<char, String> {
    let first = parse_hex4(b, at)?;
    // Not a leading surrogate: whatever it is stands alone.
    if !(0xD800..=0xDBFF).contains(&first) {
        return Ok(char::from_u32(first).unwrap_or('\u{fffd}'));
    }
    // A trailing half completes it; anything else leaves the leading half orphaned, and the
    // position must not move so that what follows is read as itself.
    let mut lookahead = *at;
    if b.get(lookahead) == Some(&b'\\') && b.get(lookahead + 1) == Some(&b'u') {
        lookahead += 2;
        if let Ok(second) = parse_hex4(b, &mut lookahead) {
            if (0xDC00..=0xDFFF).contains(&second) {
                *at = lookahead;
                let combined = 0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                return Ok(char::from_u32(combined).unwrap_or('\u{fffd}'));
            }
        }
    }
    Ok('\u{fffd}')
}

/// The four hexadecimal digits of one `\u` escape, advancing past them.
///
/// Each digit is checked rather than handed to `from_str_radix`, which accepts a leading sign and
/// would read `\u+12f` as a character nobody wrote.
fn parse_hex4(b: &[u8], at: &mut usize) -> Result<u32, String> {
    let digits = b.get(*at..*at + 4).ok_or_else(|| format!("bad \\u escape at byte {at}"))?;
    let mut value = 0u32;
    for digit in digits {
        let nibble = char::from(*digit)
            .to_digit(16)
            .ok_or_else(|| format!("bad \\u escape at byte {at}"))?;
        value = value * 16 + nibble;
    }
    *at += 4;
    Ok(value)
}

const fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn parse_int(b: &[u8], at: &mut usize) -> Result<Json, String> {
    let start = *at;
    if b.get(*at) == Some(&b'-') {
        *at += 1;
    }
    while b.get(*at).is_some_and(u8::is_ascii_digit) {
        *at += 1;
    }
    if *at == start {
        return Err(format!("not a value at byte {start}"));
    }
    // A fractional or exponent part is refused rather than truncated: silently reading 1.5 as 1
    // would put a number in a record that nobody wrote.
    if matches!(b.get(*at), Some(b'.' | b'e' | b'E')) {
        return Err(format!("fractional numbers are not part of this format, at byte {start}"));
    }
    std::str::from_utf8(&b[start..*at])
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .map(Json::Int)
        .ok_or_else(|| format!("number out of range at byte {start}"))
}

fn skip_space(b: &[u8], at: &mut usize) {
    while b.get(*at).is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')) {
        *at += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(pairs: &[(&str, Json)]) -> Json {
        Json::Object(pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect())
    }

    #[test]
    fn a_record_round_trips_through_both_halves() {
        let value = obj(&[
            ("hit", Json::Bool(true)),
            ("id", Json::Str("log is monotonic".to_owned())),
            ("line", Json::Int(220)),
            ("tags", Json::Array(vec![Json::Str("raft".to_owned()), Json::Null])),
        ]);
        let text = value.to_json();
        assert_eq!(parse(&text).expect("parses"), value);
    }

    #[test]
    fn escapes_survive_the_trip() {
        let awkward = "quote \" backslash \\ newline \n tab \t bell \u{7}";
        let value = obj(&[("message", Json::Str(awkward.to_owned()))]);
        let back = parse(&value.to_json()).expect("parses");
        assert_eq!(back.get("message").and_then(Json::as_str), Some(awkward));
    }

    #[test]
    fn text_outside_the_basic_plane_survives() {
        let value = obj(&[("emoji", Json::Str("ok ✓ 🎯".to_owned()))]);
        let back = parse(&value.to_json()).expect("parses");
        assert_eq!(back.get("emoji").and_then(Json::as_str), Some("ok ✓ 🎯"));
    }

    /// The form every serialiser that escapes non-ASCII produces, and the only form JSON has for
    /// a character outside the basic plane. Python's `json.dumps` writes it by default, so this is
    /// what arrives from a Python guest whose assertion message carries one.
    #[test]
    fn an_escaped_surrogate_pair_is_one_character_again() {
        // Written as the escape rather than as the character: the escape is the form a writer
        // that avoids non-ASCII emits, and it is the only form this test exists for. Python's
        // `json.dumps` writes every non-ASCII character this way by default.
        let back = parse(r#"{"m":"ok \ud83c\udfaf"}"#).expect("parses");
        assert_eq!(back.get("m").and_then(Json::as_str), Some("ok \u{1f3af}"));

        // And a round trip out of this crate keeps it, rather than writing the damage back.
        assert_eq!(parse(&back.to_json()).expect("parses"), back);
    }

    /// Half a pair is not a character in any encoding. It is replaced rather than refused, so one
    /// damaged message does not cost the whole record — and the halves must not swallow what
    /// follows them.
    #[test]
    fn a_lone_surrogate_becomes_one_replacement_and_eats_nothing() {
        let leading = parse(r#"{"m":"\ud83cX"}"#).expect("parses");
        assert_eq!(leading.get("m").and_then(Json::as_str), Some("\u{fffd}X"));

        let trailing = parse(r#"{"m":"\udfafX"}"#).expect("parses");
        assert_eq!(trailing.get("m").and_then(Json::as_str), Some("\u{fffd}X"));

        // A leading half followed by an escape that is not its partner keeps both meanings.
        let mismatched = parse(r#"{"m":"\ud83cA"}"#).expect("parses");
        assert_eq!(mismatched.get("m").and_then(Json::as_str), Some("\u{fffd}A"));
    }

    #[test]
    fn a_signed_or_short_escape_is_refused_rather_than_read() {
        assert!(parse(r#"{"m":"\u+041"}"#).is_err());
        assert!(parse(r#"{"m":"\u04"}"#).is_err());
        assert!(parse(r#"{"m":"\uZZZZ"}"#).is_err());
    }

    #[test]
    fn a_fractional_number_is_refused_rather_than_truncated() {
        assert!(parse("{\"n\": 1.5}").is_err());
        assert!(parse("{\"n\": 1e3}").is_err());
        assert_eq!(parse("-42").expect("parses"), Json::Int(-42));
    }

    #[test]
    fn nesting_past_the_bound_is_refused_not_followed() {
        let deep = "[".repeat(MAX_DEPTH + 5) + &"]".repeat(MAX_DEPTH + 5);
        assert!(parse(&deep).is_err(), "a document deeper than the bound must be refused");
    }

    #[test]
    fn trailing_content_is_not_ignored() {
        assert!(parse("{} extra").is_err());
        assert!(parse("{}{}").is_err());
    }

    #[test]
    fn damage_is_reported_rather_than_guessed_at() {
        for text in ["{", "[1,", "{\"a\"}", "{\"a\":}", "\"unterminated", "tr"] {
            assert!(parse(text).is_err(), "{text:?} parsed as valid");
        }
    }
}
