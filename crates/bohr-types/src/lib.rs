//! Identifiers, logical time and digests shared by every other crate.
//!
//! Nothing here performs I/O or knows anything about a domain.
//!
//! Every identifier is a newtype, so swapped arguments fail to compile. See
//! `docs/ARCHITECTURE.md` §8 rule 2.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::{borrow::Cow, fmt};

use serde::{Deserialize, Serialize};

/// Declares a transparent newtype over an integer with the traits every identifier needs.
///
/// No `Default`: zero is usually a real node or task. Types whose zero is an origin implement it
/// themselves.
macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident($inner:ty), $prefix:literal, $zero:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        #[repr(transparent)]
        pub struct $name(pub $inner);

        impl $name {
            #[doc = $zero]
            pub const ZERO: Self = Self(0);

            /// The wrapped integer.
            #[inline]
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl From<$inner> for $name {
            #[inline]
            fn from(v: $inner) -> Self {
                Self(v)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "(", $prefix, "{})"), self.0)
            }
        }
    };
}

id_newtype! {
    /// Virtual time, in nanoseconds, with no relationship to any host clock.
    ///
    /// Advanced only at syscall boundaries, switches and timer expiries.
    Tick(u64), "t", "The origin of virtual time: the tick a run starts at."
}

id_newtype! {
    /// The 64-bit number a whole run is derived from.
    Seed(u64), "seed:", "Seed nought. A run derived from it, and not an absent seed."
}

id_newtype! {
    /// A simulated machine. Owns a disk, a network identity and its own clock.
    NodeId(u32), "n", "The first node of a cluster, not a spelling of \"no node\"."
}

id_newtype! {
    /// One guest thread, as the scheduler sees it.
    ///
    /// Assigned in creation order; the host tid never reaches a trace (`docs/ARCHITECTURE.md`
    /// §4.3).
    TaskId(u32), "task", "The first task created in a run. Not a spelling of \"no task\"."
}

id_newtype! {
    /// A recorded fact's position in the run, counted from zero.
    EventId(u64), "e", "The first event recorded in a run."
}

id_newtype! {
    /// A word a guest synchronises on, numbered in the order the run first touched it.
    ///
    /// An ordinal in first-touch order, so memory layout never reaches the digest. See
    /// `docs/API.md` §4.4.
    SyncWordId(u32), "sync", "The first word the run synchronised on."
}

id_newtype! {
    /// A byte offset into the tape.
    TapeOffset(u64), "@", "The first byte of the tape."
}

// `Default` only where zero is an origin: tick, event and tape offset. `NodeId`, `TaskId` and
// `Seed` have none (§8 rule 2).
impl Default for Tick {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

impl Default for EventId {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

impl Default for TapeOffset {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

/// A guest file descriptor, virtualised at the interception boundary.
///
/// Signed, as syscall returns use `-1`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct Fd(pub i32);

impl fmt::Display for Fd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fd{}", self.0)
    }
}

impl fmt::Debug for Fd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fd(fd{})", self.0)
    }
}

/// An address inside a guest's address space.
///
/// No [`Default`]; an absent address is `Option<GuestAddr>`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct GuestAddr(pub u64);

impl fmt::Display for GuestAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl fmt::Debug for GuestAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GuestAddr({self})")
    }
}

impl Tick {
    /// One microsecond.
    pub const MICROSECOND: Self = Self(1_000);
    /// One millisecond.
    pub const MILLISECOND: Self = Self(1_000_000);
    /// One second.
    pub const SECOND: Self = Self(1_000_000_000);

    /// Advances by `delta`.
    ///
    /// Saturates, since a wrapped clock would run backwards.
    #[inline]
    #[must_use]
    pub const fn saturating_add(self, delta: Self) -> Self {
        Self(self.0.saturating_add(delta.0))
    }

    /// Ticks elapsed since `earlier`, or zero if `earlier` is in the future.
    #[inline]
    #[must_use]
    pub const fn saturating_sub(self, earlier: Self) -> Self {
        Self(self.0.saturating_sub(earlier.0))
    }

    /// This span repeated `times` over.
    ///
    /// Saturates, so a large horizon never wraps into the past.
    #[inline]
    #[must_use]
    pub const fn saturating_mul(self, times: u64) -> Self {
        Self(self.0.saturating_mul(times))
    }

    /// `seconds` seconds of virtual time.
    #[inline]
    #[must_use]
    pub const fn from_secs(seconds: u64) -> Self {
        Self::SECOND.saturating_mul(seconds)
    }

    /// `millis` milliseconds of virtual time.
    #[inline]
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self::MILLISECOND.saturating_mul(millis)
    }
}

/// A 32-byte BLAKE3 digest.
///
/// Renders as `blake3:<hex>`, so a value pasted into an issue names the hash that produced it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// Hashes `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// The raw bytes.
    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lower-case hex, without the `blake3:` prefix.
    #[must_use]
    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        // Indexed, so there is no failure path.
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(DIGITS[usize::from(b >> 4)] as char);
            s.push(DIGITS[usize::from(b & 0x0f)] as char);
        }
        s
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blake3:{}", self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Abbreviated; a full digest is unreadable in a large dump.
        write!(f, "blake3:{}…", &self.to_hex()[..12])
    }
}

/// Identity of the world model that produced a run.
///
/// Recorded in every bundle. A tool configured with a different model refuses the bundle instead of
/// replaying it. See `docs/API.md` §4.2 and `docs/ARCHITECTURE.md` §5.16.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct WorldModelId {
    /// Short stable name, such as `distributed`.
    pub name: Cow<'static, str>,
    /// Incremented when the model's behaviour changes in a way that invalidates stored runs.
    pub version: u16,
}

/// Identity of the oracle that judged a run. Same refusal rule as [`WorldModelId`].
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct OracleId {
    /// Short stable name, such as `properties+crash`.
    pub name: Cow<'static, str>,
    /// Incremented when the verdict rule changes.
    pub version: u16,
}

impl WorldModelId {
    /// Names a world model.
    ///
    /// # Panics
    ///
    /// Panics on an empty name.
    #[must_use]
    pub const fn new(name: &'static str, version: u16) -> Self {
        assert!(!name.is_empty(), "a world model's name must not be empty");
        Self { name: Cow::Borrowed(name), version }
    }
}

/// Characters [`OracleId::composed`] builds a panel's name out of.
///
/// Banned in declared names, so no oracle can be named like a panel. `+` is allowed, as in
/// `properties+crash`.
const PANEL_SYNTAX: [u8; 4] = *b"(),@";

impl OracleId {
    /// Names an oracle.
    ///
    /// # Panics
    ///
    /// Panics on an empty name or one containing `(`, `)`, `,` or `@`; in a `const` context this
    /// fails to compile.
    #[must_use]
    pub const fn new(name: &'static str, version: u16) -> Self {
        assert!(!name.is_empty(), "an oracle's name must not be empty");
        let bytes = name.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            let mut which = 0;
            while which < PANEL_SYNTAX.len() {
                assert!(
                    bytes[at] != PANEL_SYNTAX[which],
                    "an oracle's name must not contain the characters a panel's name is built from"
                );
                which += 1;
            }
            at += 1;
        }
        Self { name: Cow::Borrowed(name), version }
    }

    /// Names a panel of oracles by its members, in the order they are asked.
    ///
    /// Derived, so the bundle records what actually judged it.
    ///
    /// One member gives that member's identity. Several give `panel(first@1,second@5)`, with
    /// each member's version.
    ///
    /// # Panics
    ///
    /// Panics on an empty panel.
    #[must_use]
    pub fn composed(members: &[Self]) -> Self {
        assert!(!members.is_empty(), "a panel with no members judged nothing");
        if let [only] = members {
            return only.clone();
        }
        let mut name = String::from("panel(");
        for (at, member) in members.iter().enumerate() {
            if at > 0 {
                name.push(',');
            }
            name.push_str(&member.name);
            name.push('@');
            // Each member's own version.
            name.push_str(&member.version.to_string());
        }
        name.push(')');

        let version = members.iter().map(|m| m.version).max().unwrap_or(1);
        Self { name: Cow::Owned(name), version }
    }
}

impl fmt::Display for WorldModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

impl fmt::Display for OracleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_renders_with_its_algorithm_named() {
        let d = Digest::of(b"bohr");
        assert_eq!(d.to_hex().len(), 64);
        assert!(d.to_string().starts_with("blake3:"));
        assert_eq!(d, Digest::of(b"bohr"));
        assert_ne!(d, Digest::of(b"bohn"));
    }

    #[test]
    fn tick_saturates_instead_of_running_backwards() {
        assert_eq!(Tick(u64::MAX).saturating_add(Tick(1)), Tick(u64::MAX));
        assert_eq!(Tick(1).saturating_sub(Tick(5)), Tick::ZERO);
        assert_eq!(Tick::SECOND.get(), 1_000_000_000);
    }

    #[test]
    fn identifiers_of_different_kinds_do_not_interchange() {
        // Interchanging them would not compile; the assertions only give the test an effect.
        let (node, task) = (NodeId(3), TaskId(3));
        assert_eq!(node.get(), task.get());
        assert_eq!(node.to_string(), "n3");
        assert_eq!(task.to_string(), "task3");
    }

    /// The digest matches BLAKE3's published empty-input vector; the algorithm is part of the
    /// bundle format.
    #[test]
    fn digest_is_unkeyed_blake3_and_pinned_to_its_published_vector() {
        assert_eq!(
            Digest::of(b"").to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    /// Identifiers serialise as bare numbers.
    #[test]
    fn identifiers_are_written_as_bare_integers() {
        assert_eq!(serde_json::to_string(&NodeId(7)).expect("json"), "7");
        assert_eq!(serde_json::to_string(&TaskId(7)).expect("json"), "7");
        assert_eq!(serde_json::to_string(&Tick(7)).expect("json"), "7");
        assert_eq!(serde_json::to_string(&Fd(-1)).expect("json"), "-1");
        assert_eq!(serde_json::to_string(&GuestAddr(4096)).expect("json"), "4096");

        let back: NodeId = serde_json::from_str("7").expect("json");
        assert_eq!(back, NodeId(7));
    }

    /// A panel of one has the member's identity.
    #[test]
    fn a_panel_of_one_is_that_one() {
        let alone = OracleId::new("properties+crash", 1);
        assert_eq!(OracleId::composed(std::slice::from_ref(&alone)), alone);
    }

    /// A panel of `properties` and `crash` differs from the oracle `properties+crash`.
    #[test]
    fn a_panel_is_never_spelled_like_a_single_oracle() {
        let panel =
            OracleId::composed(&[OracleId::new("properties", 1), OracleId::new("crash", 1)]);
        assert_eq!(panel.name, "panel(properties@1,crash@1)");
        assert_ne!(panel, OracleId::new("properties+crash", 1));
    }

    /// Members at different versions give different keys.
    #[test]
    fn a_panel_carries_the_version_of_each_member() {
        let (a1, a5) = (OracleId::new("a", 1), OracleId::new("a", 5));
        let (b1, b5) = (OracleId::new("b", 1), OracleId::new("b", 5));
        assert_ne!(OracleId::composed(&[a1, b5.clone()]), OracleId::composed(&[a5, b1]));
        assert_eq!(OracleId::composed(&[OracleId::new("a", 1), b5]).version, 5);
    }

    #[test]
    #[should_panic(expected = "judged nothing")]
    fn a_panel_with_no_members_is_refused_rather_than_named() {
        let _ = OracleId::composed(&[]);
    }

    #[test]
    #[should_panic(expected = "must not contain")]
    fn an_oracle_cannot_be_named_like_a_panel() {
        let _ = OracleId::new("panel(a@1,b@1)", 1);
    }

    #[test]
    #[should_panic(expected = "must not be empty")]
    fn an_oracle_cannot_be_nameless() {
        let _ = OracleId::new("", 1);
    }

    /// Scaling saturates.
    #[test]
    fn scaling_a_span_saturates_rather_than_wrapping() {
        assert_eq!(Tick::from_secs(2), Tick(2_000_000_000));
        assert_eq!(Tick::from_millis(50), Tick(50_000_000));
        assert_eq!(Tick::SECOND.saturating_mul(u64::MAX), Tick(u64::MAX));
        assert_eq!(Tick::from_secs(u64::MAX), Tick(u64::MAX));
        assert_eq!(Tick::from_secs(0), Tick::ZERO);
    }
}
