//! Putting a guest's own words into a page without letting them become part of it.
//!
//! Every string a report shows came out of somewhere with no interest in producing valid markup:
//! an assertion's message, a diagnostic, a path, a configuration file. A report is also the
//! artefact that circulates — attached to an issue, opened by a maintainer — so a page that could
//! be made to execute something by the run it describes is the one defect here that matters.
//!
//! Three escapes rather than one, because the three places a string can land have different ways
//! of ending: an element's text ends at `<`, an attribute ends at its quote, and a string inside a
//! script block ends at `</script>` no matter what the JSON rules say about it.

/// Text between tags.
#[must_use]
pub(crate) fn text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(character),
        }
    }
    out
}

/// The value of an attribute.
///
/// Quotes as well, both kinds: an attribute written with one and closed by the other is a mistake
/// somebody will make, and escaping both costs nothing.
#[must_use]
pub(crate) fn attr(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(character),
        }
    }
    out
}

/// A JSON document that will sit inside a script element.
///
/// The real hazard of a single-file report. Inside a script block the parser is looking for
/// `</script` and nothing else — not quotes, not backslashes — so a guest that names an assertion
/// `</script><img onerror=…>` closes the block and everything after it is markup. JSON permits
/// `<` and `/` to be written as escapes, and writing them that way means the sequence never
/// appears however the guest spells it.
#[must_use]
pub(crate) fn json_in_script(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '/' => out.push_str("\\u002f"),
            // Not valid raw in a script block and not something a reader gains by seeing.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_cannot_open_an_element() {
        assert_eq!(text("<img onerror=x>"), "&lt;img onerror=x&gt;");
        assert_eq!(text("a & b"), "a &amp; b");
    }

    #[test]
    fn an_attribute_cannot_be_closed_from_inside_it() {
        assert_eq!(attr("\" onload=\"x"), "&quot; onload=&quot;x");
        assert_eq!(attr("' onload='x"), "&#39; onload=&#39;x");
    }

    /// The one that matters: a script block ends at a literal `</script`, whatever JSON thinks
    /// about escaping, so the sequence must be impossible to write.
    #[test]
    fn a_script_block_cannot_be_closed_from_inside_its_data() {
        let hostile = r#"{"name":"</script><img onerror=alert(1)>"}"#;
        let escaped = json_in_script(hostile);
        assert!(!escaped.contains("</script"), "{escaped}");
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(escaped.contains("\\u003c"), "{escaped}");
    }

    /// What comes out is still JSON: the escapes used are ones the format defines, so a reader
    /// parses the same values back.
    #[test]
    fn escaping_for_a_script_block_leaves_it_parseable() {
        let json = r#"{"a":"</script>","b":"1/2","c":" "}"#;
        let value: serde_json::Value =
            serde_json::from_str(&json_in_script(json)).expect("still json");
        assert_eq!(value["a"].as_str(), Some("</script>"));
        assert_eq!(value["b"].as_str(), Some("1/2"));
    }
}
