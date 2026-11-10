//! Where a run fell, read from what its nodes printed.
//!
//! A Go panic or runtime fatal error names its goroutine's frames; a Rust panic names the line it
//! was raised at; any other program names its failure in a fatal log line. The place is the frame
//! in the target's own code, so a finding points at a file in a release.
//!
//! Reading is by rule and knows no target: the formats are the languages' and the log levels'.

use bohr_trace::OutputTail;

/// What kind of failure a place was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `panic:` or `fatal error:`, with goroutine frames.
    GoPanic,
    /// `panicked at file:line:col`.
    RustPanic,
    /// The last line logged at a fatal or error level.
    FatalLine,
    /// The last line a failing node wrote, where it logged at no level: a program that exits with
    /// an error usually says why last.
    LastLine,
}

impl Kind {
    /// The kind in words.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::GoPanic => "Go panic",
            Self::RustPanic => "Rust panic",
            Self::FatalLine => "fatal line",
            Self::LastLine => "last line",
        }
    }
}

/// Where a node fell, and what it said as it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// The node that printed it.
    pub node: u32,
    /// Which of the node's starts printed it; 0 is the first.
    pub incarnation: u32,
    pub kind: Kind,
    /// The panic's message or the fatal line, whole; a reader clips it with [`clipped`].
    pub message: String,
    /// The first frame in the target's own code, as `file:line`, where the output names one.
    pub frame: Option<String>,
    /// The function of that frame, where the language prints it.
    pub function: Option<String>,
}

impl Place {
    /// What a finding is grouped by: the frame where there is one, else the message normalised.
    #[must_use]
    pub fn signature(&self) -> String {
        match &self.frame {
            Some(frame) => format!("{} at {frame}", self.kind.name()),
            // Normalised whole and cut after, so two lines that differ only in a number past the
            // cut still read the same.
            None => format!("{}: {}", self.kind.name(), clipped(&normalised(&self.message))),
        }
    }
}

/// Where the run fell on `nodes`, or on any node where `nodes` is empty.
///
/// The latest incarnation is read first; a panic is preferred to a fatal line, and a fatal line to
/// the failing node's last line. Without named nodes only panics count, since a healthy node logs
/// errors too.
#[must_use]
pub fn locate(tails: &[OutputTail], nodes: &[u32]) -> Option<Place> {
    let mut chosen: Vec<&OutputTail> =
        tails.iter().filter(|t| nodes.is_empty() || nodes.contains(&t.node)).collect();
    chosen.sort_by_key(|t| std::cmp::Reverse((t.incarnation, t.started_tick)));
    let panic = chosen.iter().find_map(|tail| panic_in(tail));
    if panic.is_some() || nodes.is_empty() {
        return panic;
    }
    chosen
        .iter()
        .find_map(|tail| fatal_in(tail))
        .or_else(|| chosen.iter().find_map(|tail| last_line_in(tail)))
}

/// The last line a node wrote that holds anything.
fn last_line_in(tail: &OutputTail) -> Option<Place> {
    let line = tail.text.lines().rev().find(|l| !l.trim().is_empty())?;
    Some(Place {
        node: tail.node,
        incarnation: tail.incarnation,
        kind: Kind::LastLine,
        message: line.trim().to_owned(),
        frame: None,
        function: None,
    })
}

fn panic_in(tail: &OutputTail) -> Option<Place> {
    go_panic(tail).or_else(|| rust_panic(tail))
}

/// A Go panic: its message, then the first frame whose function is not the standard library's.
fn go_panic(tail: &OutputTail) -> Option<Place> {
    let lines: Vec<&str> = tail.text.lines().collect();
    let start =
        lines.iter().rposition(|l| l.starts_with("panic: ") || l.starts_with("fatal error: "))?;
    let message = lines[start].to_owned();
    let mut function = None;
    let mut frame = None;
    // Frames come in pairs: the function, then a tab and its file and line.
    for pair in lines[start..].windows(2) {
        let (call, at) = (pair[0], pair[1]);
        if !at.starts_with('\t') || call.starts_with('\t') {
            continue;
        }
        if own_go_function(call) {
            // The argument list is the last parenthesis; a method's receiver is an earlier one.
            let name = call.trim();
            let name = match name.rfind('(') {
                Some(args) if name.ends_with(')') => &name[..args],
                _ => name,
            };
            function = Some(name.to_owned());
            frame = Some(module_path(at.trim().split(' ').next().unwrap_or("")));
            break;
        }
    }
    Some(Place {
        node: tail.node,
        incarnation: tail.incarnation,
        kind: Kind::GoPanic,
        message: message.trim().to_owned(),
        frame,
        function,
    })
}

/// Whether a Go function belongs to the program rather than the standard library.
///
/// A standard package's path has no dot in its first element; a module's does, and `main` is the
/// program's own.
fn own_go_function(call: &str) -> bool {
    let name = call.trim();
    if name.starts_with("panic(") || name.starts_with("goroutine ") || name.is_empty() {
        return false;
    }
    match name.split_once('/') {
        Some((first, _)) => first.contains('.'),
        None => name.split('.').next() == Some("main"),
    }
}

/// A Rust panic, in the format of Rust 1.73 and later or the one before it.
fn rust_panic(tail: &OutputTail) -> Option<Place> {
    let lines: Vec<&str> = tail.text.lines().collect();
    let at = lines.iter().rposition(|l| l.contains("panicked at "))?;
    let line = lines[at];
    let after = &line[line.find("panicked at ")? + "panicked at ".len()..];
    let (message, location) = if let Some(quoted) = after.strip_prefix('\'') {
        // `panicked at 'message', file:line:col`
        let end = quoted.rfind("', ")?;
        (quoted[..end].to_owned(), quoted[end + 3..].to_owned())
    } else {
        // `panicked at file:line:col:` and the message on the next line
        let location = after.trim_end_matches(':').to_owned();
        (lines.get(at + 1).map_or_else(String::new, |m| (*m).to_owned()), location)
    };
    let frame = (!rust_runtime(&location)).then(|| module_path(&location));
    Some(Place {
        node: tail.node,
        incarnation: tail.incarnation,
        kind: Kind::RustPanic,
        message: message.trim().to_owned(),
        frame,
        function: None,
    })
}

/// Whether a Rust location is in the standard library rather than the program or its crates.
fn rust_runtime(location: &str) -> bool {
    location.starts_with("/rustc/")
        || location.contains("library/core/")
        || location.contains("library/std/")
        || location.contains("library/alloc/")
}

/// The last line logged at a fatal or error level.
fn fatal_in(tail: &OutputTail) -> Option<Place> {
    let line = tail.text.lines().rev().find(|l| fatal(l))?;
    Some(Place {
        node: tail.node,
        incarnation: tail.incarnation,
        kind: Kind::FatalLine,
        message: line.trim().to_owned(),
        frame: None,
        function: None,
    })
}

/// Whether a line is logged at a fatal or error level, in the forms common logging libraries use.
fn fatal(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("Fatal:")
        || line.starts_with("FATAL")
        || line.starts_with("ERROR")
        || line.starts_with("CRIT")
        || line.starts_with("E[")
        || line.contains("level=error")
        || line.contains("level=fatal")
        || line.contains("\"level\":\"error\"")
        || line.contains("\"level\":\"fatal\"")
}

/// A source path from a module or crate cache, from the module and its version onward.
///
/// `…/pkg/mod/github.com/x/y@v1.2.3/a.go:10` keeps `github.com/x/y@v1.2.3/a.go:10`, and
/// `…/registry/src/<index>/redb-2.6.3/src/a.rs:85:47` keeps `redb-2.6.3/src/a.rs:85:47`, so the
/// path names the exact release on any machine.
fn module_path(path: &str) -> String {
    if let Some(at) = path.find("/pkg/mod/") {
        return path[at + "/pkg/mod/".len()..].to_owned();
    }
    if let Some(at) = path.find("/registry/src/") {
        let rest = &path[at + "/registry/src/".len()..];
        return rest.split_once('/').map_or(rest, |(_, crate_path)| crate_path).to_owned();
    }
    path.to_owned()
}

/// A line without its surrounding blanks, at most 200 characters.
pub fn clipped(line: &str) -> String {
    let line = line.trim();
    if line.chars().count() <= 200 {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(199).collect();
    out.push('…');
    out
}

/// A message with what differs between two runs of one defect replaced.
///
/// Numbers become `N`, hexadecimal words and long hashes `H`, and paths keep their last element:
/// the same failure at another height, address or directory reads the same.
#[must_use]
pub fn normalised(message: &str) -> String {
    let mut out = Vec::new();
    for word in message.split_whitespace() {
        let core = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '.');
        let replaced = if core.starts_with("0x")
            || (core.len() >= 16 && core.chars().all(|c| c.is_ascii_hexdigit()))
        {
            word.replace(core, "H")
        } else if core.contains('/') {
            let last = core.rsplit('/').next().unwrap_or(core);
            word.replace(core, &format!("…/{last}"))
        } else {
            word.to_owned()
        };
        let mut numbered = String::new();
        let mut in_digits = false;
        for c in replaced.chars() {
            if c.is_ascii_digit() {
                if !in_digits {
                    numbered.push('N');
                }
                in_digits = true;
            } else {
                numbered.push(c);
                in_digits = false;
            }
        }
        out.push(numbered);
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail(node: u32, incarnation: u32, text: &str) -> OutputTail {
        OutputTail { node, incarnation, started_tick: 0, dropped: 0, text: text.to_owned() }
    }

    #[test]
    fn a_go_panic_names_the_first_frame_outside_the_standard_library() {
        let text = "I[..] starting\npanic: runtime error: index out of range [5] with length 3\n\n\
                    goroutine 1 [running]:\n\
                    runtime.gopanic({0x1, 0x2})\n\t/usr/local/go/src/runtime/panic.go:770 +0x132\n\
                    github.com/cosmos/cosmos-sdk/baseapp.(*BaseApp).FinalizeBlock(0xc0001, 0x2)\n\
                    \t/root/go/pkg/mod/github.com/cosmos/cosmos-sdk@v0.53.4/baseapp/abci.go:812 +0x1d\n\
                    main.main()\n\t/src/main.go:10 +0x5\n";
        let place = locate(&[tail(2, 1, text)], &[]).expect("a panic");
        assert_eq!(place.kind, Kind::GoPanic);
        assert_eq!(place.message, "panic: runtime error: index out of range [5] with length 3");
        assert_eq!(
            place.frame.as_deref(),
            Some("github.com/cosmos/cosmos-sdk@v0.53.4/baseapp/abci.go:812")
        );
        assert_eq!(
            place.function.as_deref(),
            Some("github.com/cosmos/cosmos-sdk/baseapp.(*BaseApp).FinalizeBlock")
        );
        assert_eq!((place.node, place.incarnation), (2, 1));
    }

    #[test]
    fn a_go_program_with_its_own_main_is_its_own_frame() {
        let text = "panic: boom\n\ngoroutine 1 [running]:\nmain.run(...)\n\t/src/main.go:42 +0x1\n";
        let place = locate(&[tail(0, 0, text)], &[0]).expect("a panic");
        assert_eq!(place.frame.as_deref(), Some("/src/main.go:42"));
        assert_eq!(place.function.as_deref(), Some("main.run"));
    }

    #[test]
    fn a_rust_panic_keeps_the_crate_and_its_version() {
        let text = "thread 'main' panicked at /root/.cargo/registry/src/index.crates.io-6f17d22bba15001f/\
                    kvstore-1.4.2/src/log/segment.rs:85:47:\n\
                    index out of bounds: the len is 3 but the index is 5\n";
        let place = locate(&[tail(0, 2, text)], &[0]).expect("a panic");
        assert_eq!(place.kind, Kind::RustPanic);
        assert_eq!(place.frame.as_deref(), Some("kvstore-1.4.2/src/log/segment.rs:85:47"));
        assert_eq!(place.message, "index out of bounds: the len is 3 but the index is 5");
    }

    #[test]
    fn a_rust_panic_in_the_old_format_is_read() {
        let text = "thread 'main' panicked at 'called `Option::unwrap()` on a `None` value', src/main.rs:7:5\n";
        let place = locate(&[tail(0, 0, text)], &[0]).expect("a panic");
        assert_eq!(place.frame.as_deref(), Some("src/main.rs:7:5"));
        assert_eq!(place.message, "called `Option::unwrap()` on a `None` value");
    }

    #[test]
    fn a_panic_inside_the_standard_library_names_no_frame() {
        let text =
            "thread 'main' panicked at /rustc/abc/library/core/src/slice/index.rs:9:5:\nboom\n";
        let place = locate(&[tail(0, 0, text)], &[0]).expect("a panic");
        assert_eq!(place.frame, None);
    }

    #[test]
    fn without_a_panic_the_last_fatal_line_of_the_failing_node_is_read() {
        let first = tail(0, 0, "I[..] fine\n");
        let second = tail(
            0,
            1,
            "I[..] Version info\nERROR: failed to create node: failed to initialize database: \
             the index header is short or corrupted\n",
        );
        let place = locate(&[first, second], &[0]).expect("a fatal line");
        assert_eq!(place.kind, Kind::FatalLine);
        assert_eq!(place.incarnation, 1);
        assert!(place.message.starts_with("ERROR: failed to create node"));
    }

    #[test]
    fn without_a_panic_or_a_level_the_failing_node_s_last_words_are_read() {
        let tails = [
            tail(0, 0, ""),
            tail(0, 1, "opening\ncannot open /store.db: the header does not match\n\n"),
        ];
        let place = locate(&tails, &[0]).expect("a last line");
        assert_eq!(place.kind, Kind::LastLine);
        assert_eq!(place.message, "cannot open /store.db: the header does not match");
        assert_eq!(place.incarnation, 1);
    }

    #[test]
    fn a_fatal_line_is_not_read_without_a_failing_node() {
        let healthy = tail(1, 0, "E[2026-11-10|10:00:00] peer dropped\n");
        assert_eq!(locate(&[healthy], &[]), None);
    }

    #[test]
    fn one_defect_at_two_heights_reads_the_same() {
        let a = normalised(
            "Fatal: Could not open database: open /data/x1/kv/segment.index at 0xc00012",
        );
        let b =
            normalised("Fatal: Could not open database: open /tmp/y/kv/segment.index at 0xc00099");
        assert_eq!(a, b);
        assert_eq!(normalised("height 41 hash 9FF0D3DD1B3484FFA128DC8A"), "height N hash H");
    }
}
