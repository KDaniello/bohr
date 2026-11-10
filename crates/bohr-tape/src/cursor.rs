//! The draw ABI: the normative mapping from tape bytes to decisions.
//!
//! Byte consumption depends on the request alone, never on the value drawn, so a flipped byte
//! changes one decision and every later draw keeps its offset. Normative text: `docs/API.md` §2.4.

use bohr_types::{EventId, TapeOffset};

use crate::{
    MAX_PROBABILITY_DENOMINATOR, TapeError, draw_width_bytes,
    journal::{DrawSite, Journal, Keep},
    tape::{Tape, TapeReader},
};

/// A position on a tape, plus the journal of what has been drawn from it.
///
/// Passed explicitly to every decision, so no call site can draw ambiently. See
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

    /// The same cursor, its journal keeping only what `keep` names.
    #[must_use]
    pub fn keeping(mut self, keep: Keep) -> Self {
        self.journal = Journal::keeping(keep);
        self
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

        // The journal keeps the first eight bytes of a raw draw.
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
    /// Plain modulo at a fixed width; a rejection loop would make consumption depend on the value
    /// ([`crate::draw_width_bytes`]).
    ///
    /// The first `2^8w mod n` values are twice as likely: 1.6% excess at `n = 7`, 49.6% at
    /// `n = 129`. The worst `n` is one above a power of two. This costs search coverage, never
    /// reproduction; fixing it needs a new [`crate::TAPE_ABI`]. Pinned by
    /// `worst_case_bias_is_the_documented_one`.
    pub fn draw_range(&mut self, n: u64, site: DrawSite) -> Result<u64, TapeError> {
        if n == 0 {
            return Err(TapeError::InvalidDraw("draw_range(0)"));
        }
        let offset = TapeOffset(self.pos);
        let width = draw_width_bytes(n);
        // A one-option choice consumes nothing.
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
    /// A fixed width, so editing a probability in a config moves no stored tape.
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
        // Certainties consume nothing.
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

    /// True with probability `num/den`, consuming two bytes whatever the probability is.
    ///
    /// Unlike [`Self::draw_bool`], a certainty also costs two bytes, so overrides never change
    /// layout.
    pub fn draw_odds(&mut self, num: u32, den: u32, site: DrawSite) -> bool {
        let offset = TapeOffset(self.pos);
        let drawn = self.take_uint(2);
        let fired = den != 0 && drawn < (u64::from(num) << 16) / u64::from(den);
        self.journal.push_odds(offset, self.event, site, drawn, fired);
        fired
    }

    /// An index into `weights`, proportional to weight, consuming a fixed four bytes.
    ///
    /// A fixed width, so editing weights moves no stored tape.
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
        // Bounded so the scaling stays inside u64; a larger total is a caller bug.
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
    /// The journal records the uniform draw; the fixed-point mapping (`crate::fixed`) is pure.
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

    /// An odds draw costs two bytes at every probability, certainties and a zero denominator
    /// included, and reads and journals exactly what a range of 65,536 at that offset would.
    #[test]
    fn an_odds_draw_is_two_bytes_whatever_the_odds() {
        let tape = Tape::from_seed(Seed(9));
        for (num, den) in [(0, 1), (1, 0), (1, 1), (3, 2), (1, 2000), (1, 2)] {
            let mut odds = tape.cursor();
            let mut range = tape.cursor();
            let fired = odds.draw_odds(num, den, DrawSite::NodeFault);
            let raw = range.draw_range(1 << 16, DrawSite::NodeFault).expect("draw");
            assert_eq!(odds.position().get(), 2, "{num}/{den} cost another width");
            assert_eq!(odds.journal().records(), range.journal().records(), "{num}/{den}");
            let expected = den != 0 && raw < (u64::from(num) << 16) / u64::from(den);
            assert_eq!(fired, expected, "{num}/{den}");
            assert_eq!(odds.journal().fired(0), fired);
        }
    }

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

    /// The bias [`TapeCursor::draw_range`] documents, computed.
    #[test]
    fn worst_case_bias_is_the_documented_one() {
        let excess = |n: u64| {
            let width = crate::draw_width_bytes(n);
            let span = 1u128 << (8 * width as u32);
            (span % u128::from(n)) as f64 / span as f64
        };
        // A small scheduler draw: 1.6%.
        assert!((excess(7) - 0.015_625).abs() < 1e-9, "{}", excess(7));
        // One above a power of two is the worst case.
        assert!(excess(129) > 0.49, "the worst case moved: {}", excess(129));
        // A range dividing its width has none.
        assert!(excess(256).abs() < 1e-12);
        assert!(excess(2).abs() < 1e-12);
    }

    /// A certainty and a single-outcome weighting consume nothing, as `docs/API.md` §2.4 states.
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

        // An ordinary probability costs two bytes.
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
