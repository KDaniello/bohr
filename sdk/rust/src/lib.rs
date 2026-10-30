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
//! **Nothing here has any logic of its own.** It checks whether a channel is open, serialises a
//! record and writes a line. Everything that decides anything — whether an assertion is a finding,
//! which fault points are armed, where randomness comes from — is the supervisor's, so the SDK
//! stays something anyone could rewrite in an afternoon from `docs/API.md` §6.
//!
//! **Outside Bohr it costs one branch on a cached boolean.** A program built against this SDK and
//! run normally does not open a file, does not format a string and does not allocate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};

use bohr_sdk_proto::{json::Json, AssertRecord, BohrMeta, FaultPointRecord, Record, SDK_SCHEMA};
pub use bohr_sdk_proto::{json::Json as Value, AssertType, Location};

/// Where records are written when Bohr is running the program.
const OUTPUT_VAR: &str = "BOHR_SDK_OUTPUT";

/// The same, under the name Antithesis's own SDKs use. Honoured for compatibility (D14).
const ANTITHESIS_OUTPUT_VAR: &str = "ANTITHESIS_SDK_LOCAL_OUTPUT";

/// Which node this process is, as the supervisor numbers them.
const NODE_VAR: &str = "BOHR_SDK_NODE";

/// Fault points this run armed, comma-separated. The decision is drawn from the tape before the
/// guest starts, so the SDK only reports what it was told.
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
/// Both variables set is not an error: ours wins, and the supervisor notices the ambiguity and
/// records a diagnostic. An SDK that refused to run would break a program whose author did
/// nothing wrong.
fn open_channel() -> Option<Channel> {
    let path = std::env::var(OUTPUT_VAR)
        .ok()
        .or_else(|| std::env::var(ANTITHESIS_OUTPUT_VAR).ok())
        .filter(|p| !p.is_empty())?;

    // A bare number names an already-open descriptor. Opening it by its `/proc` path keeps this
    // crate free of `unsafe`, and a guest under Bohr is on Linux by definition.
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
    // **The number and the line it goes on are taken together.** Numbered outside the lock, two
    // threads could take five and six and then write six before five -- and the supervisor reads
    // anything but the next number as records having escaped interception, so a multithreaded
    // guest manufactured an alarm about its own evidence. The other three SDKs increment under the
    // lock they write under; the Go one says in as many words that a counter incremented outside
    // it would manufacture that alarm. This was the one that did not.
    let Ok(mut file) = channel.file.lock() else {
        return;
    };
    let meta = BohrMeta {
        schema: SDK_SCHEMA,
        seq: channel.seq.fetch_add(1, Ordering::Relaxed),
        node: channel.node,
    };
    let line = build(Some(meta)).to_line();
    // A record that cannot be written is dropped rather than raised: the SDK is a bystander in
    // someone else's program, and taking it down over a full disk would be worse than the gap the
    // supervisor will notice in the sequence numbers.
    let _ = writeln!(file, "{line}");
    let _ = file.flush();
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
                // Nothing to carry: this SDK models every field it writes.
                rest: std::collections::BTreeMap::new(),
            },
            meta,
        )
    });
}

/// Whether this run armed the named fault point, and a record that it was reached.
///
/// The answer is the supervisor's, drawn from the tape before the guest started; a program built
/// without Bohr sees `false` and takes its ordinary path.
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
/// A campaign uses this to tell "the system never started" from "the system started and then
/// broke", which are different findings with different first questions.
pub fn setup_complete(details: Json) {
    emit(move |meta| Record::SetupComplete(details, meta));
}

/// Puts a named event on the timeline.
pub fn send_event(name: &str, details: Json) {
    emit(move |meta| Record::Event(name.to_owned(), details, meta));
}

/// Random bytes that belong to the run rather than to the machine.
///
/// Under Bohr these come from the entropy tape by way of the world model, so a seed reproduces
/// them. Outside it they come from the platform, and the supervisor reports a run whose randomness
/// escaped it (`BOHR-2020`) rather than pretending the run was reproducible.
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
    // Eight bytes read as a `u64` and narrowed afterwards. Read straight into a `usize`, this did
    // not compile for a 32-bit target at all -- and an SDK is linked into somebody else's build,
    // which is compiled for more targets than a guest of this tool ever runs on.
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
    // Nothing to read from is not a reason to hand back a buffer the caller thinks is random;
    // the hasher's own seed is at least not a constant.
    use std::hash::{BuildHasher, Hasher};
    let mut state = std::collections::hash_map::RandomState::new().build_hasher();
    for (i, byte) in out.iter_mut().enumerate() {
        state.write_usize(i);
        *byte = state.finish() as u8;
    }
}

/// The name of the function this is expanded in.
///
/// Rust has no direct way to ask, so the idiom is to name an item inside the function and read
/// the path back off it. The marker's own segment is trimmed, which leaves the enclosing function
/// exactly. Defined before the macros that use it: within a crate, a `macro_rules!` is in scope
/// only after its definition.
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
    /// Disabled is the state a customer's build is normally in, and it must reach nothing.
    #[test]
    fn everything_is_inert_without_a_channel() {
        // No output variable is set in this process, so the channel resolves to none once and
        // every call below is a branch on it.
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
