//! Tell Bohr what your program promises, and let it try to break the promise.
//!
//! ```no_run
//! # struct Log;
//! # impl Log {
//! #     fn is_monotonic(&self) -> bool { true }
//! # }
//! # let log = Log;
//! # let leader_changed = true;
//! bohr_sdk::always!(log.is_monotonic(), "log is monotonic");
//! bohr_sdk::sometimes!(leader_changed, "a leader change happened");
//! if bohr_sdk::fault_point("before_commit_ack") {
//!     // this run wants the fault here
//! }
//! ```
//!
//! The SDK only checks for a channel, serialises a record and writes a line. Every decision is the
//! supervisor's, so the SDK can be rewritten from `docs/API.md` §6.
//!
//! Outside Bohr it costs one branch on a cached boolean: no file, no formatting, no allocation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use bohr_sdk_proto::{AssertRecord, BohrMeta, FaultPointRecord, Record, SDK_SCHEMA, json::Json};
pub use bohr_sdk_proto::{AssertType, Location, json::Json as Value};

/// Where records are written when Bohr is running the program.
const OUTPUT_VAR: &str = "BOHR_SDK_OUTPUT";

/// The same, under Antithesis's name, for compatibility (D14).
const ANTITHESIS_OUTPUT_VAR: &str = "ANTITHESIS_SDK_LOCAL_OUTPUT";

/// Which node this process is, as the supervisor numbers them.
const NODE_VAR: &str = "BOHR_SDK_NODE";

/// Fault points this run armed, comma-separated, drawn before the guest starts.
const ARMED_VAR: &str = "BOHR_ARMED_FAULT_POINTS";

/// The open channel, resolved once.
struct Channel {
    file: Mutex<File>,
    seq: AtomicU64,
    node: u32,
    armed: Vec<String>,
}

static CHANNEL: OnceLock<Option<Channel>> = OnceLock::new();

fn channel() -> Option<&'static Channel> {
    CHANNEL.get_or_init(open_channel).as_ref()
}

/// Opens the channel the environment names, or none.
///
/// If both variables are set, ours wins and the supervisor records a diagnostic.
fn open_channel() -> Option<Channel> {
    let path = std::env::var(OUTPUT_VAR)
        .ok()
        .or_else(|| std::env::var(ANTITHESIS_OUTPUT_VAR).ok())
        .filter(|p| !p.is_empty())?;

    // A bare number is an open descriptor, opened via `/proc` to avoid `unsafe`.
    let target = match path.parse::<u32>() {
        Ok(fd) => format!("/proc/self/fd/{fd}"),
        Err(_) => path,
    };
    let file = OpenOptions::new().create(true).append(true).open(target).ok()?;

    Some(Channel {
        file: Mutex::new(file),
        seq: AtomicU64::new(1),
        node: std::env::var(NODE_VAR).ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        armed: std::env::var(ARMED_VAR)
            .map(|v| v.split(',').filter(|s| !s.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default(),
    })
}

/// Writes one record, if anything is listening.
fn emit(build: impl FnOnce(Option<BohrMeta>) -> Record) {
    let Some(channel) = channel() else {
        return;
    };
    // Numbered and written under one lock, so sequence numbers arrive in order.
    let Ok(mut file) = channel.file.lock() else {
        return;
    };
    let meta = BohrMeta {
        schema: SDK_SCHEMA,
        seq: channel.seq.fetch_add(1, Ordering::Relaxed),
        node: channel.node,
    };
    let mut line = build(Some(meta)).to_line();
    line.push('\n');
    // A failed write is dropped; the supervisor sees the sequence gap. One write per record.
    let _ = file.write_all(line.as_bytes());
}

/// Records an assertion. Called by the macros, which capture the call site.
#[doc(hidden)]
pub fn assert_at(kind: AssertType, condition: bool, message: &str, id: &str, location: Location) {
    if channel().is_none() {
        return;
    }
    emit(|meta| {
        Record::Assert(
            AssertRecord {
                hit: true,
                must_hit: !matches!(kind, AssertType::Unreachable),
                assert_type: kind,
                message: message.to_owned(),
                condition,
                id: id.to_owned(),
                location,
                // This SDK writes no extra fields.
                rest: std::collections::BTreeMap::new(),
            },
            meta,
        )
    });
}

/// Whether this run armed the named fault point, and a record that it was reached.
///
/// Drawn by the supervisor before start; outside Bohr it is `false`.
#[must_use]
pub fn fault_point(name: &str) -> bool {
    let Some(channel) = channel() else {
        return false;
    };
    let armed = channel.armed.iter().any(|a| a == name);
    emit(|meta| Record::FaultPoint(FaultPointRecord { name: name.to_owned(), armed }, meta));
    armed
}

/// Announces that the program has finished setting itself up.
///
/// Lets a campaign tell "never started" from "started and broke".
pub fn setup_complete(details: Json) {
    emit(move |meta| Record::SetupComplete(details, meta));
}

/// Puts a named event on the timeline.
pub fn send_event(name: &str, details: Json) {
    emit(move |meta| Record::Event(name.to_owned(), details, meta));
}

/// Random bytes that belong to the run.
///
/// Under Bohr from the tape; otherwise from the platform, which the supervisor reports as
/// `BOHR-2020`.
pub fn random_bytes(out: &mut [u8]) {
    if channel().is_some() {
        let mut details = std::collections::BTreeMap::new();
        details.insert("n".to_owned(), Json::Int(out.len() as i64));
        send_event("bohr_random", Json::Object(details));
    }
    fill_random(out);
}

/// One of `choices`, or `None` when there is nothing to choose from.
pub fn random_choice<T>(choices: &[T]) -> Option<&T> {
    if choices.is_empty() {
        return None;
    }
    // Read as `u64`, then narrowed, so 32-bit targets compile.
    let mut byte = [0u8; 8];
    random_bytes(&mut byte);
    let index = u64::from_le_bytes(byte) % choices.len() as u64;
    choices.get(index as usize)
}

/// Fills a buffer from the platform's own source.
fn fill_random(out: &mut [u8]) {
    if let Ok(mut file) = File::open("/dev/urandom") {
        use std::io::Read as _;
        if file.read_exact(out).is_ok() {
            return;
        }
    }
    // Without a platform source, the hasher's seed is used.
    use std::hash::{BuildHasher, Hasher};
    let mut state = std::collections::hash_map::RandomState::new().build_hasher();
    for (i, byte) in out.iter_mut().enumerate() {
        state.write_usize(i);
        *byte = state.finish() as u8;
    }
}

/// The name of the function this is expanded in.
///
/// Read from a marker item's type path, minus its own segment. Defined before the macros that use
/// it.
#[doc(hidden)]
#[macro_export]
macro_rules! function_name {
    () => {{
        fn marker() {}
        let path = ::std::any::type_name_of_val(&marker);
        path.strip_suffix("::marker").unwrap_or(path)
    }};
}

/// Asserts that something holds every time it is reached.
#[macro_export]
macro_rules! always {
    ($cond:expr, $message:expr $(,)?) => {
        $crate::assert_at(
            $crate::AssertType::Always,
            $cond,
            $message,
            stringify!($cond),
            $crate::here(file!(), line!(), column!(), $crate::function_name!()),
        )
    };
}

/// Asserts that something holds every time it is reached, and that never reaching it is fine.
#[macro_export]
macro_rules! always_or_unreachable {
    ($cond:expr, $message:expr $(,)?) => {
        $crate::assert_at(
            $crate::AssertType::AlwaysOrUnreachable,
            $cond,
            $message,
            stringify!($cond),
            $crate::here(file!(), line!(), column!(), $crate::function_name!()),
        )
    };
}

/// Asserts that something holds at least once across a campaign.
#[macro_export]
macro_rules! sometimes {
    ($cond:expr, $message:expr $(,)?) => {
        $crate::assert_at(
            $crate::AssertType::Sometimes,
            $cond,
            $message,
            stringify!($cond),
            $crate::here(file!(), line!(), column!(), $crate::function_name!()),
        )
    };
}

/// Records that a place in the program was reached.
#[macro_export]
macro_rules! reachable {
    ($message:expr $(,)?) => {
        $crate::assert_at(
            $crate::AssertType::Reachable,
            true,
            $message,
            $message,
            $crate::here(file!(), line!(), column!(), $crate::function_name!()),
        )
    };
}

/// Records that a place that should never be reached was.
#[macro_export]
macro_rules! unreachable_point {
    ($message:expr $(,)?) => {
        $crate::assert_at(
            $crate::AssertType::Unreachable,
            false,
            $message,
            $message,
            $crate::here(file!(), line!(), column!(), $crate::function_name!()),
        )
    };
}

/// Builds a location from what the macros captured.
#[doc(hidden)]
#[must_use]
pub fn here(file: &str, line: u32, column: u32, function: &str) -> Location {
    Location {
        file: file.to_owned(),
        function: function.to_owned(),
        begin_line: line,
        begin_column: column,
        rest: std::collections::BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    /// Disabled, the normal state, does nothing.
    #[test]
    fn everything_is_inert_without_a_channel() {
        // No output variable is set, so every call is one branch.
        crate::always!(1 + 1 == 2, "arithmetic still works");
        crate::sometimes!(false, "never true here");
        crate::reachable!("this line");
        assert!(!crate::fault_point("nothing is armed"));
        crate::setup_complete(crate::Value::Null);

        let mut buf = [0u8; 16];
        crate::random_bytes(&mut buf);
        assert!(buf.iter().any(|b| *b != 0), "the fallback must produce something");
    }

    #[test]
    fn a_choice_from_nothing_is_nothing() {
        let empty: [u8; 0] = [];
        assert!(crate::random_choice(&empty).is_none());
        let one = [7u8];
        assert_eq!(crate::random_choice(&one), Some(&7));
    }
}
