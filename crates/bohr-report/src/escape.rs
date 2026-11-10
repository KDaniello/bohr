//! Putting a guest's own words into a page without letting them become part of it.
//!
//! Every string on a page comes from the guest, and reports circulate, so the run must never inject
//! markup.
//!
//! Three escapes for three contexts: element text ends at `<`, an attribute at its quote, and a
//! script block at `</script>`.

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
/// Both quote kinds are escaped.
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
/// A script block ends at `</script` regardless of JSON quoting, so `<` and `/` are written as JSON
/// escapes and the sequence never appears.
#[must_use]
pub(crate) fn json_in_script(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '/' => out.push_str("\\u002f"),
            // Invalid raw in a script block.
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

    /// `</script` cannot appear in the output.
    #[test]
    fn a_script_block_cannot_be_closed_from_inside_its_data() {
        let hostile = r#"{"name":"</script><img onerror=alert(1)>"}"#;
        let escaped = json_in_script(hostile);
        assert!(!escaped.contains("</script"), "{escaped}");
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(escaped.contains("\\u003c"), "{escaped}");
    }

    /// The output is still JSON and parses to the same values.
    #[test]
    fn escaping_for_a_script_block_leaves_it_parseable() {
        let json = r#"{"a":"</script>","b":"1/2","c":" "}"#;
        let value: serde_json::Value =
            serde_json::from_str(&json_in_script(json)).expect("still json");
        assert_eq!(value["a"].as_str(), Some("</script>"));
        assert_eq!(value["b"].as_str(), Some("1/2"));
    }
}
