//! Identifiers, logical time and digests shared by every other crate.
//!
//! Nothing here performs I/O or knows anything about a domain.
//!
//! Every identifier is a newtype rather than a bare integer: two `u64` parameters that can be
//! swapped eventually are, and a swapped identifier yields a plausible but wrong trace. See
//! `docs/ARCHITECTURE.md` §8 rule 2.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::{borrow::Cow, fmt};

use serde::{Deserialize, Serialize};

/// Declares a transparent newtype over an integer with the traits every identifier needs.
///
/// A macro rather than repeated derives: hand-written variants drift apart.
///
/// `Default` is deliberately not among them. Zero is a real inhabitant of most of these types —
/// the first node, the first task — so a derived default is a value that reads as "unset" and
/// compares equal to something that exists. `unwrap_or_default()` on a missing identifier is then
/// a plausible wrong answer rather than an error, which is the failure this crate's whole reason
/// for existing is meant to exclude. Where zero genuinely is an origin, the type says so itself.
///
/// Each type states what its own zero means: a constant every type shares and no type explains is
/// a constant every reader guesses at.
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
    /// Advanced only at defined points — a syscall boundary, a scheduler switch, a timer expiry.
    /// A guest that sleeps for an hour wakes instantly having advanced this by 3.6e12.
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
    /// Virtual and assigned by us in creation order. The host tid is host-specific and must never
    /// reach a trace — see `docs/ARCHITECTURE.md` §4.3.
    TaskId(u32), "task", "The first task created in a run. Not a spelling of \"no task\"."
}

id_newtype! {
    /// A recorded fact's position in the run, counted from zero.
    EventId(u64), "e", "The first event recorded in a run."
}

id_newtype! {
    /// A word a guest synchronises on, numbered in the order the run first touched it.
    ///
    /// **An ordinal rather than the address itself.** A futex word is a guest virtual address, and
    /// an address moves when the memory around it moves — a heap-allocated lock sits somewhere
    /// else the moment an earlier mapping changes size. Numbering the words in first-touch order
    /// keeps what a trace needs (this is a different word from that one) and drops what it must
    /// not have (where the word happened to live on this machine), so two runs that behaved alike
    /// digest alike. See `docs/API.md` §4.4.
    SyncWordId(u32), "sync", "The first word the run synchronised on."
}

id_newtype! {
    /// A byte offset into the tape.
    TapeOffset(u64), "@", "The first byte of the tape."
}

// `Default` belongs to the three types whose zero is an origin rather than an inhabitant: a run
// starts at tick nought, records its first event at nought and reads its tape from nought, so
// defaulting to those is defaulting to the beginning. `NodeId`, `TaskId` and `Seed` are left
// without one on purpose — their zero is a particular node, a particular task and a particular
// run, and a default that silently names one of them is the plausible wrong answer §8 rule 2
// exists to prevent. Written out rather than generated, because each line is that decision.
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
/// Signed because the guest's own numbering is, and because syscall returns use `-1`.
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
/// No [`Default`]: the address it would produce is the null pointer, and a defaulted null is how
/// a buffer nobody supplied comes to be read as a buffer at address zero. Where an address is
/// genuinely absent, `Option<GuestAddr>` says so.
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
    /// Saturates rather than wraps: a wrapped clock runs time backwards and breaks the delivery
    /// queue's ordering invariant.
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
    /// Here rather than at the call sites: scaling a unit is what every horizon, budget and
    /// timeout is written as, and doing it on the wrapped integer drops the saturation the two
    /// operations above are careful to keep. A wrapped product is a horizon in the past.
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
        // Indexed rather than converted, so there is no failure path to substitute a character
        // for: a digest that could render a placeholder mid-string would hide the corruption a
        // digest exists to reveal.
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
    /// Panics on an empty name. The name is half of the key a replay refuses on, and a nameless
    /// model would make every bundle it wrote refusable against every other nameless one.
    #[must_use]
    pub const fn new(name: &'static str, version: u16) -> Self {
        assert!(!name.is_empty(), "a world model's name must not be empty");
        Self { name: Cow::Borrowed(name), version }
    }
}

/// Characters [`OracleId::composed`] builds a panel's name out of.
///
/// Banned from a declared name so that the composed form cannot be spelled by hand: the name is
/// the key a replay refuses on, and a key two different panels could share is a refusal that can
/// be argued with. `+` is deliberately absent — `properties+crash` is one oracle that does two
/// things, and renaming it would refuse every bundle already written under it.
const PANEL_SYNTAX: [u8; 4] = [b'(', b')', b',', b'@'];

impl OracleId {
    /// Names an oracle.
    ///
    /// # Panics
    ///
    /// Panics on an empty name, or on one containing any of `(`, `)`, `,` or `@` — the syntax
    /// [`Self::composed`] builds a panel's name from. Both are programming errors in wiring
    /// rather than anything a user can cause, and the name is a `&'static str`, so one written in
    /// a `const` context fails to compile rather than at run time.
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
    /// Derived rather than declared, so what a bundle records is what actually judged it. A name
    /// written by hand beside a list that grew is a bundle claiming a judgement nobody made — and
    /// this is the field a replay refuses on, so the lie would be enforced.
    ///
    /// One member gives exactly that member's identity, which is what keeps every bundle written
    /// before panels existed replaying against a panel of one.
    ///
    /// Several give `panel(first@1,second@5)`: each member carries its own version, because a
    /// panel whose members are the same but at different versions judged the run differently and
    /// must not share a key. The surrounding syntax is banned from a declared name by
    /// [`Self::new`], so no single oracle can be spelled to collide with a panel.
    ///
    /// # Panics
    ///
    /// Panics on an empty panel. Nothing judged the run, so there is no identity to record, and
    /// the nameless one this used to return was refusable against every other empty panel.
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
            // The member's own version, decimal, because a panel of the same names at different
            // versions is a different panel and a `max` over them would hide that.
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

    /// The algorithm is a bundle-format promise, not an implementation detail: every stored
    /// digest was produced by it. The vector is BLAKE3's own published one for the empty input,
    /// taken from the specification rather than from this crate, so a switch to a keyed or
    /// derived mode fails here instead of silently invalidating every bundle ever written.
    #[test]
    fn digest_is_unkeyed_blake3_and_pinned_to_its_published_vector() {
        assert_eq!(
            Digest::of(b"").to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    /// `#[serde(transparent)]` on every identifier is what makes a manifest readable by anything
    /// other than this build. A lost attribute would wrap each one in an object and change the
    /// bundle format without changing a version.
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

    /// A panel of one is the member itself, which is what lets a bundle written before panels
    /// existed replay against a panel of one.
    #[test]
    fn a_panel_of_one_is_that_one() {
        let alone = OracleId::new("properties+crash", 1);
        assert_eq!(OracleId::composed(std::slice::from_ref(&alone)), alone);
    }

    /// The refusal key has to tell two different judgements apart. `properties+crash` is a real
    /// oracle that does two things, so the plain join this replaced gave a panel of `properties`
    /// and `crash` the identity of an oracle that already exists.
    #[test]
    fn a_panel_is_never_spelled_like_a_single_oracle() {
        let panel =
            OracleId::composed(&[OracleId::new("properties", 1), OracleId::new("crash", 1)]);
        assert_eq!(panel.name, "panel(properties@1,crash@1)");
        assert_ne!(panel, OracleId::new("properties+crash", 1));
    }

    /// Members at different versions judged the run differently, so the two orderings must not
    /// share a key. Taking a maximum over the versions alone gave both the same one.
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

    /// Every horizon and timeout in the tool is a unit scaled by a number, and the scaling used
    /// to happen on the wrapped integer. A horizon that wrapped would sit in the past.
    #[test]
    fn scaling_a_span_saturates_rather_than_wrapping() {
        assert_eq!(Tick::from_secs(2), Tick(2_000_000_000));
        assert_eq!(Tick::from_millis(50), Tick(50_000_000));
        assert_eq!(Tick::SECOND.saturating_mul(u64::MAX), Tick(u64::MAX));
        assert_eq!(Tick::from_secs(u64::MAX), Tick(u64::MAX));
        assert_eq!(Tick::from_secs(0), Tick::ZERO);
    }
}
