//! Where the search's own decisions come from.
//!
//! The explorer's choices (which entry, which operator, which byte) come off a tape, like the
//! guest's, so a campaign repeats.
//!
//! The tape is separate from any run's, so changing the search never changes a stored tape.

use bohr_tape::Tape;
use bohr_types::Seed;

/// Bytes read from the tape at a time.
///
/// `Tape::read_at` builds a cipher per call, so bytes are read a block at a time.
const BLOCK_BYTES: usize = 256;

/// A deterministic stream of choices, derived from one seed.
#[derive(Debug)]
pub struct Choices {
    tape: Tape,
    at: u64,
    /// Bytes already read from the tape and not yet handed out, and how many are left.
    block: [u8; BLOCK_BYTES],
    held: usize,
}

impl Choices {
    /// A stream for one piece of work.
    #[must_use]
    pub fn new(seed: Seed) -> Self {
        Self { tape: Tape::from_seed(seed), at: 0, block: [0u8; BLOCK_BYTES], held: 0 }
    }

    /// The next byte.
    pub fn byte(&mut self) -> u8 {
        if self.held == 0 {
            self.tape.read_at(self.at, &mut self.block);
            self.at += BLOCK_BYTES as u64;
            self.held = BLOCK_BYTES;
        }
        // In tape order, so buffering is invisible.
        let byte = self.block[BLOCK_BYTES - self.held];
        self.held -= 1;
        byte
    }

    /// The next four bytes as a number.
    pub fn word(&mut self) -> u32 {
        let out = [self.byte(), self.byte(), self.byte(), self.byte()];
        u32::from_le_bytes(out)
    }

    /// A number below `n`, or zero where there is nothing to choose between.
    ///
    /// Always four bytes, whatever `n` is, so consumption never depends on the value.
    pub fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        self.word() as usize % n
    }

    /// One of the given items, or nothing where there are none.
    pub fn pick<'a, T>(&mut self, from: &'a [T]) -> Option<&'a T> {
        from.get(self.below(from.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_seed_is_one_sequence_of_choices() {
        let mut first = Choices::new(Seed(7));
        let mut again = Choices::new(Seed(7));
        let mut other = Choices::new(Seed(8));

        let a: Vec<u8> = (0..64).map(|_| first.byte()).collect();
        let b: Vec<u8> = (0..64).map(|_| again.byte()).collect();
        let c: Vec<u8> = (0..64).map(|_| other.byte()).collect();

        assert_eq!(a, b, "the search must decide the same way twice");
        assert_ne!(a, c, "two seeds that decided alike would search one path twice");
    }

    #[test]
    fn a_choice_stays_inside_its_range() {
        let mut choices = Choices::new(Seed(1));
        for n in [1usize, 2, 3, 17, 256, 1000] {
            for _ in 0..64 {
                assert!(choices.below(n) < n, "a choice below {n} was not");
            }
        }
        assert_eq!(choices.below(0), 0, "nothing to choose between is not a choice");
        assert!(choices.pick::<u8>(&[]).is_none());
    }
}
