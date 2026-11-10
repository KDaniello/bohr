//! The value a run is compared by.
//!
//! Implementations produce different event streams, so they are compared by a projection: a value
//! the specification requires to be equal, read from the world after the run.
//!
//! A projection is configuration, so the guest needs no SDK; it names a file or the output.
//!
//! The value must be equal under any legal schedule, such as a state root after N blocks. Event
//! digests and orderings are not. See `docs/ARCHITECTURE.md` §5.20.

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
    /// Often the caller's node.
    pub node: u32,
    /// Where its bytes come from, within that node.
    pub source: Source,
}

/// The most a projection may hold.
///
/// Past it the value is marked truncated and not compared.
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
