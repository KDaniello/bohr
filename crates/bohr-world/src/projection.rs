//! The value a run is compared by.
//!
//! Two implementations of one specification, driven by one tape, do not produce the same event
//! stream — different languages, different syscalls, different internals — so their digests cannot
//! be compared and something else has to be. That something is a **projection**: a value the
//! specification requires to be equal, extracted from the world after the run.
//!
//! **The guest does nothing.** A projection is a rule in the configuration, not a channel a program
//! opts into, which is the whole reason it works against a client that will never adopt an SDK. The
//! world already holds a filesystem and already sees what a guest writes to its output; a
//! projection names which of that to read.
//!
//! What may be compared is bounded by an argument rather than by taste, and it is the argument that
//! makes the comparison sound at all: **the value must be one the specification requires to be
//! equal under any legal schedule.** A state root after N blocks is such a value. An event digest,
//! a log ordering, anything timing-sensitive is not — and comparing one of those would report a
//! disagreement on nearly every seed, which is a machine for producing false findings. See
//! `docs/ARCHITECTURE.md` §5.20.

use std::collections::BTreeMap;

/// Where a projection's bytes are read from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// A file on the node's own filesystem, by the path a guest would open it at.
    File(String),
    /// Everything the guest wrote to its standard output.
    Stdout,
}

/// One value, and where to find it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProjectionSpec {
    /// What this value is called, in the configuration and in the report.
    pub label: String,
    /// Which node produced it.
    ///
    /// **Not always the implementation's own.** A client that has to be driven leaves the record
    /// of what it did with the caller that drove it, so the value under comparison is on a
    /// node other than the one being compared — and reading it off node zero by convention
    /// finds nothing at all, which is a comparison that did not happen rather than one that
    /// failed.
    pub node: u32,
    /// Where its bytes come from, within that node.
    pub source: Source,
}

/// The most a projection may hold.
///
/// Past it the value is marked truncated and is **not** compared: two cut-off values that match
/// agree about nothing, and calling that agreement is worse than reporting that the question went
/// unanswered.
pub const MAX_PROJECTION_BYTES: usize = 1 << 20;

/// What one execution produced, by label.
#[derive(Clone, Debug, Default)]
pub struct Projected {
    values: BTreeMap<String, Value>,
}

/// One extracted value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Value {
    /// The bytes, up to the bound.
    pub bytes: Vec<u8>,
    /// Whether there were more of them than may be held.
    pub truncated: bool,
}

impl Projected {
    /// Records what one label came to.
    pub fn insert(&mut self, label: String, value: Value) {
        self.values.insert(label, value);
    }

    /// What a label came to, where it was found at all.
    #[must_use]
    pub fn get(&self, label: &str) -> Option<&Value> {
        self.values.get(label)
    }

    /// Every label, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.values.iter()
    }

    /// Whether anything was found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}
