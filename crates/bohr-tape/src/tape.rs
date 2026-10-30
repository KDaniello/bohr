//! The tape itself: a finite prefix plus a deterministic extension that does not run out.

use bohr_types::{Digest, Seed};
use chacha20::{
    cipher::{KeyIvInit, StreamCipher, StreamCipherSeek},
    ChaCha8,
};

use crate::{cursor::TapeCursor, TapeError, TAPE_ABI, TAPE_FORMAT_VERSION};

/// First four bytes of a `.tape` container.
pub const TAPE_MAGIC: [u8; 4] = *b"BTP\x01";

/// Size of the `.tape` header, before the prefix bytes.
pub const TAPE_HEADER_BYTES: usize = 64;

/// Domain separator for the extension key. Part of the frozen ABI: changing this string changes
/// every run of every stored seed.
const EXTENSION_CONTEXT: &[u8] = b"bohr/tape/extension/v1";

/// How far one keying of the extension cipher is used for.
///
/// **The cipher has a ceiling and the tape claimed not to.** ChaCha8 with a 96-bit nonce counts
/// blocks in 32 bits, so one keying reaches `2^32` blocks of 64 bytes — 256 GiB — and the crate
/// panics rather than erring past it. A tape documented as endless that panics inside the
/// supervisor destroys the run it exists to record.
///
/// Half the ceiling rather than all of it, because the last block of a keying is not dependable
/// here: measured against `cipher` 0.4.4, seeking to `2^38 - 64` and applying a whole block
/// panics, while seeking to `2^38 - 8` and applying eight bytes does not. Staying an entire
/// keying away from that block costs nothing — 128 GiB is beyond any run that will ever exist —
/// and buys not having to reason about which side of the quirk a read lands on.
///
/// Past a segment the nonce carries its number, so the extension goes on being a pure function of
/// the seed. Segment nought takes the all-zero nonce, which is exactly what the single keying
/// used, so every byte any stored run has ever read is unchanged and [`crate::TAPE_ABI`] is
/// untouched.
const EXTENSION_SEGMENT_BYTES: u64 = 1 << 37;

/// The nonce a segment of the extension is keyed with.
///
/// Little-endian in the low eight bytes, the rest zero: segment nought is the all-zero nonce.
fn segment_nonce(segment: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&segment.to_le_bytes());
    nonce
}

/// A finite prefix followed by a deterministic keystream that does not run out.
///
/// Past the end of the prefix the tape keeps producing bytes, still as a pure function of the
/// seed, for as far as an offset can be counted — the cipher is re-keyed per
/// `EXTENSION_SEGMENT_BYTES` rather than asked for more than one keying can give.
#[derive(Clone)]
pub struct Tape {
    seed: Seed,
    prefix: Vec<u8>,
    extension_key: [u8; 32],
}

impl Tape {
    /// A tape with no prefix: every byte comes from the seed.
    #[must_use]
    pub fn from_seed(seed: Seed) -> Self {
        Self::with_prefix(seed, Vec::new())
    }

    /// A tape whose first `prefix.len()` bytes are given, the rest derived from the seed.
    #[must_use]
    pub fn with_prefix(seed: Seed, prefix: Vec<u8>) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(EXTENSION_CONTEXT);
        hasher.update(&seed.get().to_le_bytes());
        Self { seed, prefix, extension_key: *hasher.finalize().as_bytes() }
    }

    /// The seed this tape was derived from.
    #[inline]
    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.seed
    }

    /// The mutable prefix.
    #[inline]
    #[must_use]
    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    /// Digest of the prefix, as recorded in the container header.
    #[must_use]
    pub fn prefix_digest(&self) -> Digest {
        Digest::of(&self.prefix)
    }

    /// Fills `out` with the tape bytes starting at absolute offset `from`.
    ///
    /// One-shot: the extension cipher is built and discarded. A run reads through
    /// [`Self::reader`] instead, which keeps it.
    pub fn read_at(&self, from: u64, out: &mut [u8]) {
        self.reader().read_at(from, out);
    }

    /// A sequential reader over the tape.
    #[must_use]
    pub fn reader(&self) -> TapeReader<'_> {
        TapeReader { tape: self, cipher: None, segment: 0, next_ext: 0 }
    }

    /// A cursor positioned at the start of the tape.
    #[must_use]
    pub fn cursor(&self) -> TapeCursor<'_> {
        TapeCursor::new(self)
    }

    /// Serialises the container: a 64-byte header followed by the prefix.
    #[must_use]
    pub fn to_container(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(TAPE_HEADER_BYTES + self.prefix.len());
        out.extend_from_slice(&TAPE_MAGIC);
        out.extend_from_slice(&TAPE_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&TAPE_ABI.to_le_bytes());
        out.extend_from_slice(&self.seed.get().to_le_bytes());
        out.extend_from_slice(&(self.prefix.len() as u64).to_le_bytes());
        out.extend_from_slice(self.prefix_digest().as_bytes());
        out.extend_from_slice(&[0u8; 8]); // reserved
        debug_assert_eq!(out.len(), TAPE_HEADER_BYTES);
        out.extend_from_slice(&self.prefix);
        out
    }

    /// Parses a container, verifying the magic, both versions, the reserved bytes, the exact
    /// length and the prefix hash.
    ///
    /// Every check is fatal: a silently misread tape produces a plausible, wrong run. That
    /// includes bytes past the declared prefix — nothing would ever read them, so a container
    /// carrying them was not written by this tool.
    pub fn from_container(bytes: &[u8]) -> Result<Self, TapeError> {
        if bytes.len() < TAPE_HEADER_BYTES {
            return Err(TapeError::HeaderTooShort { actual: bytes.len() });
        }
        let magic: [u8; 4] = bytes[0..4].try_into().unwrap_or_default();
        if magic != TAPE_MAGIC {
            return Err(TapeError::BadMagic { found: magic, expected: TAPE_MAGIC });
        }
        let format = u16::from_le_bytes([bytes[4], bytes[5]]);
        if format != TAPE_FORMAT_VERSION {
            return Err(TapeError::UnsupportedFormatVersion {
                found: format,
                supported: TAPE_FORMAT_VERSION,
            });
        }
        // Any other ABI, not merely a newer one. The same bytes read under another draw ABI are
        // another run, and an older interpretation is no more this build's than a newer one is:
        // accepting it would produce a run that is self-consistent and not the one recorded.
        let abi = u16::from_le_bytes([bytes[6], bytes[7]]);
        if abi != TAPE_ABI {
            return Err(TapeError::UnsupportedAbi { found: abi, supported: TAPE_ABI });
        }
        if bytes[56..64].iter().any(|b| *b != 0) {
            return Err(TapeError::ReservedNotZero);
        }

        let seed = Seed(u64::from_le_bytes(bytes[8..16].try_into().unwrap_or_default()));
        let declared = u64::from_le_bytes(bytes[16..24].try_into().unwrap_or_default());
        let available = (bytes.len() - TAPE_HEADER_BYTES) as u64;
        if declared > available {
            return Err(TapeError::Truncated { declared, actual: available });
        }
        if declared < available {
            return Err(TapeError::TrailingBytes { declared, actual: available });
        }

        let prefix = bytes[TAPE_HEADER_BYTES..TAPE_HEADER_BYTES + declared as usize].to_vec();
        let expected = Digest(bytes[24..56].try_into().unwrap_or_default());
        let tape = Self::with_prefix(seed, prefix);
        let computed = tape.prefix_digest();
        if computed != expected {
            return Err(TapeError::PrefixHashMismatch { expected, computed });
        }
        Ok(tape)
    }
}

/// Reads tape bytes while holding on to the extension cipher between calls.
///
/// The cipher is a stream: advancing it costs nothing, seeking it costs a block computation, and
/// building it costs a key schedule. A run draws a few bytes at a time and always moves forward, so
/// keeping one reader turns what would be one cipher setup per draw into one per run.
pub struct TapeReader<'a> {
    tape: &'a Tape,
    /// Positioned at `next_ext` within `segment`, once anything has been read from the extension.
    cipher: Option<ChaCha8>,
    /// Which [`EXTENSION_SEGMENT_BYTES`] block of the extension `cipher` is keyed for.
    segment: u64,
    next_ext: u64,
}

impl TapeReader<'_> {
    /// Fills `out` with the tape bytes starting at absolute offset `from`.
    ///
    /// Reads spanning the prefix boundary are stitched here rather than by callers: an off-by-one
    /// in the split changes how the tape decodes at exactly one length.
    pub fn read_at(&mut self, from: u64, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }
        let prefix_len = self.tape.prefix.len() as u64;
        let mut written = 0usize;

        if from < prefix_len {
            let available = (prefix_len - from) as usize;
            let take = available.min(out.len());
            let start = from as usize;
            out[..take].copy_from_slice(&self.tape.prefix[start..start + take]);
            written = take;
        }
        if written == out.len() {
            return;
        }

        // Extension offsets are counted from the end of the prefix, not from the start of the
        // tape, so a corpus entry keeps its meaning when the prefix is extended by a mutator.
        let ext_from = (from + written as u64).saturating_sub(prefix_len);
        self.fill_extension(ext_from, &mut out[written..]);
    }

    /// ChaCha8 keystream at `from`, counted from the start of the extension.
    ///
    /// Split across segments where the read crosses one, because a segment is as far as one
    /// keying of the cipher reaches ([`EXTENSION_SEGMENT_BYTES`]).
    fn fill_extension(&mut self, from: u64, out: &mut [u8]) {
        let mut written = 0usize;
        let mut at = from;
        while written < out.len() {
            let segment = at / EXTENSION_SEGMENT_BYTES;
            let within = at % EXTENSION_SEGMENT_BYTES;
            let room = (EXTENSION_SEGMENT_BYTES - within).min((out.len() - written) as u64);
            // The cast is bounded by `out.len()`, which the line above took the minimum against.
            let take = room as usize;

            // A cipher keyed for another segment answers about another part of the tape, so it is
            // rebuilt rather than sought. Within one segment, seeking is what the kept reader is
            // for: a run draws forward and pays one key schedule instead of one per draw.
            if self.cipher.is_none() || self.segment != segment {
                self.cipher = Some(ChaCha8::new(
                    &self.tape.extension_key.into(),
                    &segment_nonce(segment).into(),
                ));
                self.segment = segment;
                self.next_ext = 0;
            }
            let cipher = self.cipher.as_mut().expect("keyed for this segment just above");
            if within != self.next_ext {
                cipher.seek(within);
                self.next_ext = within;
            }

            let piece = &mut out[written..written + take];
            piece.fill(0);
            cipher.apply_keystream(piece);
            self.next_ext += room;
            written += take;
            at += room;
        }
    }
}

impl std::fmt::Debug for TapeReader<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapeReader").field("next_ext", &self.next_ext).finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Tape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tape")
            .field("seed", &self.seed)
            .field("prefix_len", &self.prefix.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_is_a_pure_function_of_the_seed() {
        let a = Tape::from_seed(Seed(42));
        let b = Tape::from_seed(Seed(42));
        let c = Tape::from_seed(Seed(43));
        let (mut x, mut y, mut z) = ([0u8; 64], [0u8; 64], [0u8; 64]);
        a.read_at(0, &mut x);
        b.read_at(0, &mut y);
        c.read_at(0, &mut z);
        assert_eq!(x, y, "same seed must give the same bytes");
        assert_ne!(x, z, "different seeds must not collide on 64 bytes");
    }

    #[test]
    fn reads_are_seekable_and_agree_with_sequential_reads() {
        let tape = Tape::with_prefix(Seed(7), vec![1, 2, 3, 4, 5]);
        let mut whole = [0u8; 40];
        tape.read_at(0, &mut whole);
        for split in 0..whole.len() {
            let mut tail = vec![0u8; whole.len() - split];
            tape.read_at(split as u64, &mut tail);
            assert_eq!(&whole[split..], &tail[..], "seek to {split} disagrees with sequential");
        }
    }

    #[test]
    fn a_read_straddling_the_prefix_boundary_is_stitched() {
        let prefix = vec![0xAA; 5];
        let tape = Tape::with_prefix(Seed(1), prefix);
        let mut out = [0u8; 10];
        tape.read_at(0, &mut out);
        assert_eq!(&out[..5], &[0xAA; 5], "prefix comes first, verbatim");

        let mut ext_only = [0u8; 5];
        tape.read_at(5, &mut ext_only);
        assert_eq!(&out[5..], &ext_only, "the extension continues where the prefix ended");
    }

    #[test]
    fn a_kept_reader_agrees_with_a_fresh_one() {
        let tape = Tape::with_prefix(Seed(11), vec![7; 9]);
        let mut reader = tape.reader();

        // Forward in small steps, the way a run draws, then backwards, which forces a seek.
        for pos in (0..64u64).chain((0..64u64).rev()) {
            let mut kept = [0u8; 5];
            reader.read_at(pos, &mut kept);
            let mut fresh = [0u8; 5];
            tape.reader().read_at(pos, &mut fresh);
            assert_eq!(kept, fresh, "reader state changed what offset {pos} reads");
        }
    }

    #[test]
    fn container_round_trips_and_rejects_damage() {
        let tape = Tape::with_prefix(Seed(0xDEAD_BEEF), (0..200u32).map(|i| i as u8).collect());
        let bytes = tape.to_container();
        let back = Tape::from_container(&bytes).expect("round trip");
        assert_eq!(back.seed(), tape.seed());
        assert_eq!(back.prefix(), tape.prefix());

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(matches!(Tape::from_container(&bad_magic), Err(TapeError::BadMagic { .. })));

        let mut bad_reserved = bytes.clone();
        bad_reserved[56] = 1;
        assert!(matches!(Tape::from_container(&bad_reserved), Err(TapeError::ReservedNotZero)));

        let mut future_abi = bytes.clone();
        future_abi[6] = 99;
        assert!(matches!(Tape::from_container(&future_abi), Err(TapeError::UnsupportedAbi { .. })));

        let mut other_format = bytes.clone();
        other_format[4] = 2;
        assert!(matches!(
            Tape::from_container(&other_format),
            Err(TapeError::UnsupportedFormatVersion { .. })
        ));

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(Tape::from_container(&trailing), Err(TapeError::TrailingBytes { .. })));

        let mut corrupt = bytes.clone();
        corrupt[TAPE_HEADER_BYTES + 3] ^= 0x01;
        assert!(matches!(
            Tape::from_container(&corrupt),
            Err(TapeError::PrefixHashMismatch { .. })
        ));
    }

    /// A tape written under another reading of its bytes is another tape, whichever direction the
    /// difference points. An older ABI is not a subset of this one: it is a different mapping, and
    /// reading it here would produce a run that looks self-consistent and is not the one recorded.
    #[test]
    fn a_tape_under_any_other_abi_is_refused_in_both_directions() {
        let bytes = Tape::with_prefix(Seed(3), vec![1, 2, 3]).to_container();

        let mut older = bytes.clone();
        older[6] = 0;
        assert!(matches!(Tape::from_container(&older), Err(TapeError::UnsupportedAbi { .. })));

        let mut newer = bytes.clone();
        newer[6] = 2;
        assert!(matches!(Tape::from_container(&newer), Err(TapeError::UnsupportedAbi { .. })));

        assert!(Tape::from_container(&bytes).is_ok(), "the ABI this build writes still reads");
    }

    /// The extension is a pure function of the seed at any offset, including past the point one
    /// keying of the cipher reaches. Before this was split into segments, reading here panicked
    /// inside the cipher crate — a panic in the supervisor, which destroys the run it was keeping.
    #[test]
    fn the_extension_goes_on_past_one_keying_of_the_cipher() {
        let tape = Tape::from_seed(Seed(17));
        for from in [
            EXTENSION_SEGMENT_BYTES - 8,
            EXTENSION_SEGMENT_BYTES,
            EXTENSION_SEGMENT_BYTES + 1,
            EXTENSION_SEGMENT_BYTES * 3 + 5,
            u64::MAX - 64,
        ] {
            let (mut once, mut again) = ([0u8; 32], [0u8; 32]);
            tape.read_at(from, &mut once);
            tape.read_at(from, &mut again);
            assert_eq!(once, again, "offset {from} did not repeat");
        }
    }

    /// A read that straddles the boundary between two keyings must be the two pieces it is made
    /// of, or the split is a place where the tape means one thing read whole and another read in
    /// parts.
    #[test]
    fn a_read_across_a_segment_boundary_is_stitched() {
        let tape = Tape::from_seed(Seed(23));
        let start = EXTENSION_SEGMENT_BYTES - 16;
        let mut whole = [0u8; 32];
        tape.read_at(start, &mut whole);

        let (mut head, mut tail) = ([0u8; 16], [0u8; 16]);
        tape.read_at(start, &mut head);
        tape.read_at(EXTENSION_SEGMENT_BYTES, &mut tail);
        assert_eq!(&whole[..16], &head, "the piece before the boundary moved");
        assert_eq!(&whole[16..], &tail, "the piece after the boundary moved");
    }

    /// Every byte a stored run has ever read comes from the first segment, and its keying is the
    /// one a single-keyed cipher used. This is what makes re-keying past the ceiling a change no
    /// stored tape can notice.
    #[test]
    fn the_first_segment_is_keyed_exactly_as_one_keying_was() {
        assert_eq!(segment_nonce(0), [0u8; 12], "segment nought must take the all-zero nonce");
        assert_ne!(segment_nonce(1), [0u8; 12]);
    }

    #[test]
    fn truncation_at_every_offset_is_reported_never_ignored() {
        let tape = Tape::with_prefix(Seed(5), vec![9; 32]);
        let bytes = tape.to_container();
        for cut in 0..bytes.len() {
            assert!(
                Tape::from_container(&bytes[..cut]).is_err(),
                "a container cut at {cut} must not parse as valid"
            );
        }
        assert!(Tape::from_container(&bytes).is_ok());
    }
}
