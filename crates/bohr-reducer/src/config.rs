//! The world configuration, as a document that can be made smaller.
//!
//! Edited as text, so the reduced file keeps the author's comments and order and the change reads
//! as a diff.
//!
//! It knows only where each array-of-tables entry begins and ends and which lines carry a tick:
//! enough to remove an entry or bring it forward.

/// A configuration document, kept as the lines it was written as.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConfigDoc {
    lines: Vec<String>,
    entries: Vec<Entry>,
}

/// One `[[table]]` entry: something the world was told to do at a moment.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The table it belongs to — `partition`, `grey`, `fault`.
    pub table: String,
    /// Lines it occupies, header included.
    span: std::ops::Range<usize>,
}

/// A moment a reduction can bring forward.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Moment {
    /// The line it is written on.
    line: usize,
    /// The key, for a report that says what was changed.
    pub key: String,
    /// What it says now.
    pub ticks: u64,
}

/// Keys that name a moment the world acts at.
///
/// Listed by name: `latency_mean_ticks` is also ticks, but lowering it makes a different scenario.
const MOMENT_KEYS: [&str; 4] = ["at_tick", "heal_at_tick", "until_tick", "restart_after_ticks"];

impl ConfigDoc {
    /// Reads a document.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let mut entries = Vec::new();
        let mut open: Option<(String, usize)> = None;

        // Depth of the value being read, so lines inside a multi-line array, such as
        // `[[0, 1], [2, 3]]`, are never read as headers.
        let mut depth = 0usize;
        for (at, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let inside_value = depth > 0;
            depth = depth_after(line, depth);
            if inside_value {
                continue;
            }
            if trimmed.starts_with('[')
                && let Some((table, from)) = open.take()
            {
                entries.push(Entry { table, span: from..at });
            }
            if let Some(name) = trimmed.strip_prefix("[[").and_then(|rest| rest.strip_suffix("]]"))
            {
                open = Some((name.trim().to_owned(), at));
            }
        }
        if let Some((table, from)) = open {
            entries.push(Entry { table, span: from..lines.len() });
        }
        Self { lines, entries }
    }

    /// The entries, in the order they are written.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The moments the world acts at, in the order they are written.
    #[must_use]
    pub fn moments(&self) -> Vec<Moment> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(at, line)| {
                let (key, value) = line.split_once('=')?;
                let key = key.trim();
                if !MOMENT_KEYS.contains(&key) {
                    return None;
                }
                let ticks = value.trim().replace('_', "").parse().ok()?;
                Some(Moment { line: at, key: key.to_owned(), ticks })
            })
            .collect()
    }

    /// The document with only the entries at these positions kept.
    ///
    /// Everything else, such as `[network]`, comments and blank lines, is left as it was.
    #[must_use]
    pub fn keeping(&self, keep: &[usize]) -> Self {
        let dropped: Vec<&Entry> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(at, _)| !keep.contains(at))
            .map(|(_, entry)| entry)
            .collect();
        let lines: Vec<String> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(at, _)| !dropped.iter().any(|entry| entry.span.contains(at)))
            .map(|(_, line)| line.clone())
            .collect();
        Self::parse(&join(&lines))
    }

    /// The document with one moment brought forward.
    #[must_use]
    pub fn moved(&self, moment: &Moment, to: u64) -> Self {
        let mut lines = self.lines.clone();
        let line = &lines[moment.line];
        let indent = line.len() - line.trim_start().len();
        // Whatever follows the value, such as a comment, stays.
        let trailing =
            line.split_once('#').map(|(_, note)| format!("  #{note}")).unwrap_or_default();
        lines[moment.line] = format!("{}{} = {to}{trailing}", " ".repeat(indent), moment.key);
        Self::parse(&join(&lines))
    }

    /// The document as the text a bundle stores and a replay re-parses.
    #[must_use]
    pub fn to_toml(&self) -> String {
        join(&self.lines)
    }
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

fn join(lines: &[String]) -> String {
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENARIO: &str = "\
# a comment somebody wrote
[network]
topology = \"full\"
drop_probability = \"1/1000\"

[[partition]]
at_tick = 9_000_000_000
heal_at_tick = 18446744073709551615
cut = { from = [0], to = [1] }

[[fault]]
kind = \"node_kill\"
node = 1
at_tick = 9_000_000_100

[disk]
stall_probability = \"1/500\"
";

    #[test]
    fn the_entries_a_reduction_can_remove_are_found() {
        let doc = ConfigDoc::parse(SCENARIO);
        let tables: Vec<&str> = doc.entries().iter().map(|e| e.table.as_str()).collect();
        assert_eq!(tables, vec!["partition", "fault"]);
    }

    /// Removing an entry removes its lines and nothing else.
    #[test]
    fn removing_an_entry_leaves_the_rest_of_the_file_alone() {
        let doc = ConfigDoc::parse(SCENARIO);
        let without_fault = doc.keeping(&[0]);
        let text = without_fault.to_toml();

        assert!(!text.contains("node_kill"), "the fault survived: {text}");
        assert!(text.contains("[[partition]]"), "the partition was taken too: {text}");
        assert!(text.contains("# a comment somebody wrote"), "a comment was lost: {text}");
        assert!(text.contains("stall_probability"), "the disk was lost: {text}");
        assert_eq!(without_fault.entries().len(), 1);
    }

    #[test]
    fn removing_everything_leaves_a_world_with_nothing_scheduled() {
        let text = ConfigDoc::parse(SCENARIO).keeping(&[]).to_toml();
        assert!(!text.contains("[["), "an entry survived: {text}");
        assert!(text.contains("[network]"), "the network went with them: {text}");
    }

    /// A fault's moment is found and rewritten, including with `_` separators.
    #[test]
    fn a_moment_is_found_and_brought_forward() {
        let doc = ConfigDoc::parse(SCENARIO);
        let moments = doc.moments();
        let at_ticks: Vec<u64> = moments.iter().map(|m| m.ticks).collect();
        assert_eq!(at_ticks, vec![9_000_000_000, 18_446_744_073_709_551_615, 9_000_000_100]);

        let sooner = doc.moved(&moments[0], 1);
        assert!(sooner.to_toml().contains("at_tick = 1"), "{}", sooner.to_toml());
        assert!(sooner.to_toml().contains("node_kill"), "another entry was disturbed");
        assert_eq!(sooner.moments()[0].ticks, 1);
    }

    /// A tick count that is not a moment is left alone.
    #[test]
    fn a_duration_is_not_a_moment() {
        let doc = ConfigDoc::parse("[network]\nlatency_mean_ticks = 500_000\n");
        assert!(doc.moments().is_empty(), "a latency was taken for a moment");
    }

    #[test]
    fn a_document_survives_being_read_and_written() {
        let doc = ConfigDoc::parse(SCENARIO);
        assert_eq!(doc.to_toml(), SCENARIO);
    }
}
