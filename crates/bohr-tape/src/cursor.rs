//! The draw ABI: the normative mapping from tape bytes to decisions.
//!
//! Byte consumption is a function of the request alone, never of the value drawn. A flipped byte
//! therefore changes one decision and leaves every later draw reading from the same offset; if
//! consumption depended on the value, one flip would shift everything downstream. Every definition
//! here follows from that rule. Normative text: `docs/API.md` §2.4.

use bohr_types::{EventId, TapeOffset};

use crate::{
    draw_width_bytes,
    journal::{DrawSite, Journal},
    tape::{Tape, TapeReader},
    TapeError, MAX_PROBABILITY_DENOMINATOR,
};

/// A position on a tape, plus the journal of what has been drawn from it.
///
/// Threaded by hand into every place a decision is made. The inconvenience is deliberate: an
/// ambient generator would remove determinism without any call site changing. See
/// `docs/ARCHITECTURE.md` §5.1.
pub struct TapeCursor<'a> {
    reader: TapeReader<'a>,
    pos: u64,
    event: EventId,
    journal: Journal,
}

impl<'a> TapeCursor<'a> {
    pub(crate) fn new(tape: &'a Tape) -> Self {
        Self { reader: tape.reader(), pos: 0, event: EventId::ZERO, journal: Journal::new() }
    }

    /// Current offset.
    #[inline]
    #[must_use]
    pub const fn position(&self) -> TapeOffset {
        TapeOffset(self.pos)
    }

    /// Stamps subsequent draws with the event they belong to.
    #[inline]
    pub fn set_event(&mut self, event: EventId) {
        self.event = event;
    }

    /// The draws made so far.
    #[inline]
    #[must_use]
    pub const fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Consumes the cursor, yielding its journal.
    #[must_use]
    pub fn into_journal(self) -> Journal {
        self.journal
    }

    /// Raw bytes into `out`. The primitive every other draw is built from.
    pub fn draw_into(&mut self, out: &mut [u8], site: DrawSite) {
        let offset = TapeOffset(self.pos);
        self.reader.read_at(self.pos, out);
        #[cfg(feature = "chaos-ambient-rng")]
        crate::chaos::overwrite(out);
        self.pos += out.len() as u64;

        // A raw draw can be a whole page; the journal keeps its first eight bytes so a report has
        // something to show without carrying the payload twice.
        let mut head = [0u8; 8];
        let take = out.len().min(8);
        head[..take].copy_from_slice(&out[..take]);
        self.journal.push(offset, self.event, site, out.len() as u32, u64::from_le_bytes(head));
    }

    /// `n` bytes little-endian, zero-extended to `u64`. `n` above 8 is a caller bug.
    pub fn draw_uint(&mut self, n: usize, site: DrawSite) -> Result<u64, TapeError> {
        if n > 8 {
            return Err(TapeError::InvalidDraw("draw_uint width above 8 bytes"));
        }
        let value = self.take_uint(n);
        self.journal.push(TapeOffset(self.pos - n as u64), self.event, site, n as u32, value);
        Ok(value)
    }

    /// Uniform in `0..n`.
    ///
    /// Plain modulo at a fixed width, with no rejection loop: a retry consumes extra bytes and so
    /// makes consumption depend on the drawn value, which is the one property the whole ABI rests
    /// on ([`crate::draw_width_bytes`]).
    ///
    /// **The bias this buys is not small at every `n`.** The first `2^8w mod n` values come up
    /// twice as often as the rest, so the excess mass is `(2^8w mod n) / 2^8w`: 1.6% at `n = 7`,
    /// and 49.6% at `n = 129`, where 127 of the 129 values are twice as likely as the other two.
    /// The worst `n` for a given width is one more than a power of two, and `draw_index` over 129
    /// runnable tasks is an ordinary shape for a threaded guest.
    ///
    /// What that costs is search coverage, never reproduction: the mapping is a pure function of
    /// the bytes, so a run repeats whatever the skew. Removing it means widening the draw, which
    /// re-reads every stored tape, so it belongs to a change of [`crate::TAPE_ABI`] rather than
    /// here. Pinned by `worst_case_bias_is_the_documented_one` so the number in this paragraph
    /// cannot drift from the code.
    pub fn draw_range(&mut self, n: u64, site: DrawSite) -> Result<u64, TapeError> {
        if n == 0 {
            return Err(TapeError::InvalidDraw("draw_range(0)"));
        }
        let offset = TapeOffset(self.pos);
        let width = draw_width_bytes(n);
        // A choice with one option is not a decision, and charging it a byte would make tape
        // layout depend on how many degenerate choices a run happened to contain.
        let value = if width == 0 { 0 } else { self.take_uint(width) % n };
        self.journal.push(offset, self.event, site, width as u32, value);
        Ok(value)
    }

    /// An index into a slice of length `len`.
    pub fn draw_index(&mut self, len: usize, site: DrawSite) -> Result<usize, TapeError> {
        let n = u64::try_from(len).map_err(|_| TapeError::InvalidDraw("slice longer than u64"))?;
        Ok(self.draw_range(n, site)? as usize)
    }

    /// True with probability `num/den`, consuming a fixed two bytes whatever the probability is.
    ///
    /// Not `draw_range(den) < num`: that ties the width to the denominator, so editing a
    /// probability literal in a config re-lays-out every tape in the corpus.
    pub fn draw_bool(&mut self, num: u32, den: u32, site: DrawSite) -> Result<bool, TapeError> {
        if den == 0 {
            return Err(TapeError::InvalidDraw("draw_bool with zero denominator"));
        }
        if den > MAX_PROBABILITY_DENOMINATOR {
            return Err(TapeError::InvalidDraw(
                "probability denominator above 65536; see docs/API.md §5.2",
            ));
        }
        let offset = TapeOffset(self.pos);
        // Certainties consume nothing, by the same reasoning as a one-option choice.
        let (value, width) = if num == 0 {
            (0u64, 0usize)
        } else if num >= den {
            (1u64, 0usize)
        } else {
            let threshold = (u64::from(num) << 16) / u64::from(den);
            let drawn = self.take_uint(2);
            (u64::from(drawn < threshold), 2usize)
        };
        self.journal.push(offset, self.event, site, width as u32, value);
        Ok(value == 1)
    }

    /// An index into `weights`, proportional to weight, consuming a fixed four bytes.
    ///
    /// Same hazard and same fix as [`Self::draw_bool`]: `draw_range(sum)` would tie the width to
    /// weight magnitudes, and weights are configuration.
    pub fn draw_weighted(&mut self, weights: &[u32], site: DrawSite) -> Result<usize, TapeError> {
        let mut sum: u64 = 0;
        let mut nonzero = 0usize;
        let mut last_nonzero = 0usize;
        for (i, w) in weights.iter().enumerate() {
            sum = sum.saturating_add(u64::from(*w));
            if *w != 0 {
                nonzero += 1;
                last_nonzero = i;
            }
        }
        if sum == 0 {
            return Err(TapeError::InvalidDraw("draw_weighted with all-zero weights"));
        }
        // Bounded so the scaling below stays inside u64; a larger total is a caller bug, not a
        // case to silently widen for.
        if sum > u64::from(u32::MAX) {
            return Err(TapeError::InvalidDraw("draw_weighted total above u32::MAX"));
        }

        let offset = TapeOffset(self.pos);
        if nonzero == 1 {
            self.journal.push(offset, self.event, site, 0, last_nonzero as u64);
            return Ok(last_nonzero);
        }

        let scaled = self.take_uint(4);
        let target = (scaled * sum) >> 32;
        let mut running: u64 = 0;
        let mut chosen = last_nonzero;
        for (i, w) in weights.iter().enumerate() {
            running += u64::from(*w);
            if running > target {
                chosen = i;
                break;
            }
        }
        self.journal.push(offset, self.event, site, 4, chosen as u64);
        Ok(chosen)
    }

    /// An exponentially distributed value with the given mean, consuming a fixed four bytes.
    ///
    /// The journal records the uniform draw, not the mapped value: the mapping is a pure function
    /// evaluated in fixed point (`crate::fixed`), so a mutator edits the underlying uniform and
    /// the journal stays regenerable from the tape alone.
    pub fn draw_exponential(&mut self, mean: u64, site: DrawSite) -> Result<u64, TapeError> {
        let u = self.draw_range(1 << 32, site)?;
        Ok(crate::fixed::exp_inverse_cdf(mean, u))
    }

    /// Reads `n` bytes and advances, without journalling. Callers journal with the right width.
    fn take_uint(&mut self, n: usize) -> u64 {
        debug_assert!(n <= 8, "take_uint width is bounded by the caller");
        let mut buf = [0u8; 8];
        self.reader.read_at(self.pos, &mut buf[..n]);
        #[cfg(feature = "chaos-ambient-rng")]
        crate::chaos::overwrite(&mut buf[..n]);
        self.pos += n as u64;
        u64::from_le_bytes(buf)
    }
}

impl std::fmt::Debug for TapeCursor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapeCursor")
            .field("pos", &self.pos)
            .field("event", &self.event)
            .field("draws", &self.journal.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use bohr_types::Seed;

    use super::*;

    #[test]
    fn an_exponential_draw_costs_four_bytes_and_journals_the_uniform() {
        let tape = Tape::from_seed(Seed(9));
        let mut cursor = tape.cursor();
        let value = cursor.draw_exponential(1_000_000, DrawSite::NetDelay).expect("draw");
        assert_eq!(cursor.position().get(), 4, "cost is fixed whatever the mean");

        let record = &cursor.journal().records()[0];
        assert_eq!(record.len, 4);
        assert_ne!(record.value, value, "the journal holds the uniform, not the mapped value");
        assert_eq!(crate::fixed::exp_inverse_cdf(1_000_000, record.value), value);
    }

    /// The number the doc comment on [`TapeCursor::draw_range`] states, computed rather than
    /// asserted from memory. A bound written in prose drifts from the code the first time either
    /// moves; this is what keeps the paragraph honest.
    #[test]
    fn worst_case_bias_is_the_documented_one() {
        let excess = |n: u64| {
            let width = crate::draw_width_bytes(n);
            let span = 1u128 << (8 * width as u32);
            (span % u128::from(n)) as f64 / span as f64
        };
        // The scheduler's small draws, where the skew is the 1.6% the documentation quotes.
        assert!((excess(7) - 0.015_625).abs() < 1e-9, "{}", excess(7));
        // One more than a power of two is the worst case for a width, and it is not small.
        assert!(excess(129) > 0.49, "the worst case moved: {}", excess(129));
        // A range that divides its width exactly has none at all.
        assert!(excess(256).abs() < 1e-12);
        assert!(excess(2).abs() < 1e-12);
    }

    /// A certainty and a single-outcome weighting consume nothing, which is a discontinuity in an
    /// ABI whose whole point is that consumption does not move. It is deliberate — a decision with
    /// one outcome is not a decision — and it is pinned here because `docs/API.md` §2.4 states it
    /// and nothing else would notice it changing.
    #[test]
    fn a_certainty_is_not_a_decision_and_costs_nothing() {
        let tape = Tape::from_seed(Seed(4));
        let mut cursor = tape.cursor();

        assert!(!cursor.draw_bool(0, 1000, DrawSite::NetDeliver).expect("draw"), "never is never");
        assert_eq!(cursor.position().get(), 0, "an impossibility read a byte");
        assert!(cursor.draw_bool(1000, 1000, DrawSite::NetDeliver).expect("draw"), "always always");
        assert_eq!(cursor.position().get(), 0, "a certainty read a byte");
        assert!(
            cursor.draw_bool(7, 5, DrawSite::NetDeliver).expect("draw"),
            "past certain is certain"
        );
        assert_eq!(cursor.position().get(), 0);

        // The same for a weighting with one outcome, and it names that outcome.
        assert_eq!(cursor.draw_weighted(&[0, 3, 0], DrawSite::NodeFault).expect("draw"), 1);
        assert_eq!(cursor.position().get(), 0, "a single-outcome weighting read a byte");

        // And an ordinary probability still costs its fixed two bytes.
        let _ = cursor.draw_bool(1, 2, DrawSite::NetDeliver).expect("draw");
        assert_eq!(cursor.position().get(), 2);
    }

    #[test]
    fn a_zero_mean_still_consumes_its_bytes() {
        let tape = Tape::from_seed(Seed(9));
        let mut cursor = tape.cursor();
        let value = cursor.draw_exponential(0, DrawSite::NetDelay).expect("draw");
        assert_eq!(value, 0);
        assert_eq!(cursor.position().get(), 4, "consumption is a function of the request alone");
    }
}
