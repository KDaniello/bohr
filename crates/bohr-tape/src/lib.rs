//! The entropy tape: the single source of nondeterminism in a run.
//!
//! A tape is a finite prefix followed by a deterministic extension derived from the seed, which
//! goes as far as an offset can be counted. The prefix is what a mutator operates on; the
//! extension means a run that draws more than expected keeps going instead of failing.
//!
//! This crate assigns no meaning to a draw. It hands out bytes and records offsets; interpretation
//! belongs to the kernel and the world model.
//!
//! The byte-level contract is normative in `docs/API.md` §2 and frozen by [`TAPE_ABI`]. Changing it
//! re-interprets every stored corpus and every stored finding.
//!
//! ## Layout
//!
//! - [`Tape`] — the bytes and the `.tape` container
//! - [`TapeCursor`] — the draw ABI
//! - [`Journal`] — what was drawn, for the reducer and the report

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(feature = "chaos-ambient-rng")]
mod chaos;
mod cursor;
mod fixed;
mod journal;
mod tape;

pub use cursor::TapeCursor;
pub use journal::{DRAW_RECORD_BYTES, DrawRecord, DrawSite, Journal};
pub use tape::{TAPE_HEADER_BYTES, TAPE_MAGIC, Tape, TapeReader};

/// Version of the `.tape` container layout.
pub const TAPE_FORMAT_VERSION: u16 = 1;

/// Version of the draw ABI: the mapping from tape bytes to decisions.
///
/// A bundle recording a different value is refused rather than replayed. A run derived from
/// differently interpreted bytes is a different run. See `docs/API.md` §9.
pub const TAPE_ABI: u16 = 1;

/// Largest denominator the boolean draw can express.
///
/// A finer probability is rejected rather than rounded to never. See `docs/API.md` §5.2.
pub const MAX_PROBABILITY_DENOMINATOR: u32 = 65_536;

/// Failures that originate in the tape layer.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TapeError {
    /// `BOHR-4001` — the container is not a tape this tool recognises.
    #[error("BOHR-4001: not a bohr tape (magic {found:02x?}, expected {expected:02x?})")]
    BadMagic {
        /// The four bytes actually found.
        found: [u8; 4],
        /// The four bytes required.
        expected: [u8; 4],
    },

    /// `BOHR-4001` — a container layout this tool does not implement.
    #[error("BOHR-4001: tape container format {found} is not one this tool reads ({supported})")]
    UnsupportedFormatVersion {
        /// The layout version recorded in the container.
        found: u16,
        /// The layout version this build implements.
        supported: u16,
    },

    /// `BOHR-4005` — written under a draw ABI this tool does not implement.
    #[error("BOHR-4005: tape ABI {found} is not the one this tool reads ({supported})")]
    UnsupportedAbi {
        /// The ABI recorded in the container.
        found: u16,
        /// The ABI this build implements.
        supported: u16,
    },

    /// `BOHR-4003` — a field reserved for future use was not zero.
    #[error("BOHR-4003: reserved header bytes are not zero")]
    ReservedNotZero,

    /// `BOHR-4002` — the prefix does not hash to what the header claims.
    #[error("BOHR-4002: prefix hash mismatch (header {expected}, computed {computed})")]
    PrefixHashMismatch {
        /// The digest recorded in the header.
        expected: bohr_types::Digest,
        /// The digest of the bytes actually present.
        computed: bohr_types::Digest,
    },

    /// `BOHR-4001` — the container is shorter than a header.
    #[error(
        "BOHR-4001: tape truncated: {actual} bytes is shorter than the {TAPE_HEADER_BYTES}-byte header"
    )]
    HeaderTooShort {
        /// Bytes actually present.
        actual: usize,
    },

    /// The container is shorter than its own header says it is.
    #[error("BOHR-4002: tape truncated: header declares {declared} prefix bytes, {actual} present")]
    Truncated {
        /// Prefix length from the header.
        declared: u64,
        /// Prefix bytes actually present.
        actual: u64,
    },

    /// `BOHR-4002` — the container is longer than its own header says it is.
    #[error("BOHR-4002: tape has {actual} bytes after the header, header declares {declared}")]
    TrailingBytes {
        /// Prefix length from the header.
        declared: u64,
        /// Bytes actually present after the header.
        actual: u64,
    },

    /// `BOHR-5002` — the caller asked for something that is never a valid draw.
    #[error("BOHR-5002: invalid draw request: {0}")]
    InvalidDraw(&'static str),
}

/// Bytes consumed by a draw over `0..n`, as a function of `n` alone.
///
/// Independence from the drawn value is what lets a mutated byte change one decision instead of
/// shifting every decision after it. See `docs/API.md` §2.4.
#[inline]
#[must_use]
pub const fn draw_width_bytes(n: u64) -> usize {
    if n <= 1 {
        return 0;
    }
    let bits = 64 - (n - 1).leading_zeros();
    bits.div_ceil(8) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_depends_only_on_the_range() {
        assert_eq!(draw_width_bytes(0), 0);
        assert_eq!(draw_width_bytes(1), 0, "a choice with one option is not a decision");
        assert_eq!(draw_width_bytes(2), 1);
        assert_eq!(draw_width_bytes(256), 1);
        assert_eq!(draw_width_bytes(257), 2);
        assert_eq!(draw_width_bytes(65_536), 2);
        assert_eq!(draw_width_bytes(65_537), 3);
        assert_eq!(draw_width_bytes(1 << 32), 4);
        assert_eq!(draw_width_bytes(u64::MAX), 8);
    }
}
