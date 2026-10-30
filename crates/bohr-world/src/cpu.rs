//! The processor the guest is shown.
//!
//! One invented machine, identical on every host. A guest asks what it is running on and gets the
//! same answer here and in a data centre, which is what lets a bundle produced on one machine
//! replay on another — goal T4. The real processor is never consulted, so nothing in this table
//! depends on the silicon under it.
//!
//! **What is advertised, a guest may execute.** Promise `AVX-512` and a guest that takes that path
//! dies on a host that has none, so the profile is a level and the level is a host requirement:
//! `bohr doctor` refuses a machine that cannot do what this promises. The level is `x86-64-v3` —
//! `AVX`, `AVX2`, `BMI`, `FMA` on top of `SSE4.2` — which is what a machine built since 2013
//! carries and what the targets this exists for are compiled against. Under-promising was tried
//! first and does not work: `solana_perf` refuses to start without `AVX` and says so.
//!
//! **`RDRAND` is absent from this table on purpose.** Clearing the bit sends most libraries down
//! the path that asks the operating system for randomness instead, which this world answers from
//! the tape. It does not stop a program that executes the instruction without asking first —
//! that is `DETERMINISM.md` §2.2 and is closed elsewhere — but it is the cheap majority of the
//! problem.
//!
//! Every number here is part of what a run *is*: a guest branches on them. The golden test at the
//! foot of this file is what makes changing one a decision rather than an accident.

/// What the guest reads from leaf zero, twelve bytes across `ebx`, `edx` and `ecx`.
///
/// A real vendor rather than an invented one, because libraries branch on this string and the
/// branches for the two real vendors are the ones that get tested. The choice is a one-way door:
/// every bundle ever written records the machine its run was shown.
const VENDOR: &[u8; 12] = b"GenuineIntel";

/// What the guest reads from the extended leaves: forty-eight bytes, zero-padded.
///
/// Named for what it is. A guest that prints its processor must not be able to claim it ran on
/// hardware it did not, and a reader looking for a frequency finds one rather than falling over.
const BRAND: &[u8; 48] =
    b"bohr virtual x86-64 CPU @ 1.00GHz\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";

/// Highest basic leaf answered.
const MAX_BASIC: u32 = 0x0000_000D;

/// Highest extended leaf answered.
const MAX_EXTENDED: u32 = 0x8000_0008;

/// Feature bits in `ecx` of leaf one.
///
/// `SSE3`, `PCLMULQDQ`, `FMA`, `SSSE3`, `CMPXCHG16B`, `SSE4.1`, `SSE4.2`, `MOVBE`, `POPCNT`,
/// `XSAVE`, `OSXSAVE`, `AVX` and `F16C`. Spelled bit by bit rather than copied from a real
/// processor: bit thirty is `RDRAND`, and a copied word would carry it back one day with nobody
/// noticing.
const FEATURES_ECX: u32 = (1 << 0)
    | (1 << 1)
    | (1 << 12)
    | (1 << 9)
    | (1 << 13)
    | (1 << 19)
    | (1 << 20)
    | (1 << 22)
    | (1 << 23)
    | (1 << 26)
    | (1 << 27)
    | (1 << 28)
    | (1 << 29);

/// Feature bits in `ebx` of leaf seven: `BMI1`, `AVX2` and `BMI2`, and nothing above them.
const EXTENDED_EBX: u32 = (1 << 3) | (1 << 5) | (1 << 8);

/// Feature bits in `edx` of leaf one: the original set every x86-64 processor carries, up to
/// `SSE2`. Bit twenty-eight, which says one core holds several threads, is clear.
const FEATURES_EDX: u32 = 0x078B_FBFF;

/// The four registers one identification fills, in `eax`, `ebx`, `ecx`, `edx` order.
pub type Registers = [u32; 4];

/// What the processor answers for one leaf and sub-leaf.
///
/// A leaf this profile does not model reads as four zeros rather than as the highest leaf's
/// answer. The difference is deliberate: zeros are read by every caller as "not present", where
/// repeating another leaf's contents would answer one question with a fact about a different one.
#[must_use]
pub fn identify(leaf: u32, subleaf: u32) -> Registers {
    match (leaf, subleaf) {
        (0x0000_0000, _) => [MAX_BASIC, word(VENDOR, 0), word(VENDOR, 8), word(VENDOR, 4)],

        // Version, then what one core of it is. `ebx` carries the cache line in eight-byte units,
        // one logical processor and identifier zero: this world runs a guest on one core and says
        // so here as well as in `/proc/cpuinfo` and in `sysconf`. A thread pool sized from a
        // disagreement between the three would be a pool this world cannot schedule.
        (0x0000_0001, _) => [0x0003_06C1, 0x0001_0800, FEATURES_ECX, FEATURES_EDX],

        // Structured extended features. `RDSEED` is bit eighteen and stays clear for the same
        // reason `RDRAND` does in the leaf above.
        (0x0000_0007, 0) => [0, EXTENDED_EBX, 0, 0],

        // The saved state area, which a program reads before it may use the wide registers.
        // `x87`, `SSE` and `AVX`; the size of the area and the offset the vector half sits at.
        (0x0000_000D, 0) => [0x0000_0007, 0x0000_0340, 0x0000_0340, 0],
        (0x0000_000D, 2) => [0x0000_0100, 0x0000_0240, 0, 0],

        (0x8000_0000, _) => [MAX_EXTENDED, 0, 0, 0],

        // Long mode, no-execute and `syscall`, without which a sixty-four bit program cannot run
        // at all, plus `lahf` in `ecx`.
        (0x8000_0001, _) => [0, 0, 0x0000_0021, 0x2010_0800],

        (0x8000_0002, _) => brand(0),
        (0x8000_0003, _) => brand(16),
        (0x8000_0004, _) => brand(32),

        // The second-level cache, which `glibc` reads to choose the width its copies work in.
        // Half a megabyte, eight-way, sixty-four byte lines.
        (0x8000_0006, _) => [0, 0, 0x0200_6040, 0],

        // Physical and virtual address widths: thirty-nine and forty-eight, the ordinary pair.
        (0x8000_0008, _) => [0x0000_3027, 0, 0, 0],

        _ => [0, 0, 0, 0],
    }
}

/// Four bytes of a fixed string as the little-endian word a register carries them in.
const fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Sixteen bytes of the brand string as the four registers one extended leaf carries.
const fn brand(at: usize) -> Registers {
    [word(BRAND, at), word(BRAND, at + 4), word(BRAND, at + 8), word(BRAND, at + 12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendor reads back as itself, in the register order the instruction uses — which is not
    /// alphabetical, and is the mistake this test exists to catch.
    #[test]
    fn leaf_zero_spells_the_vendor() {
        let [max, ebx, ecx, edx] = identify(0, 0);
        let mut spelled = Vec::new();
        spelled.extend_from_slice(&ebx.to_le_bytes());
        spelled.extend_from_slice(&edx.to_le_bytes());
        spelled.extend_from_slice(&ecx.to_le_bytes());
        assert_eq!(spelled, VENDOR);
        assert_eq!(max, MAX_BASIC);
    }

    /// The one bit this whole profile exists to clear.
    ///
    /// A guest told the instruction is there uses it, and what it returns is real entropy no seed
    /// governs. Clearing the bit is the cheap majority of that problem, and a profile that set it
    /// would undo the expensive half along with it.
    #[test]
    fn hardware_randomness_is_absent() {
        assert_eq!(identify(1, 0)[2] & (1 << 30), 0, "RDRAND in leaf 1 ecx");
        assert_eq!(identify(7, 0)[1] & (1 << 18), 0, "RDSEED in leaf 7 ebx");
    }

    /// One core, said here as well as in `/proc/cpuinfo` and in `sysconf`.
    #[test]
    fn the_guest_is_shown_one_processor() {
        let [_, ebx, _, edx] = identify(1, 0);
        assert_eq!((ebx >> 16) & 0xFF, 1, "logical processors in leaf 1 ebx");
        assert_eq!(edx & (1 << 28), 0, "the several-threads-per-core bit");
    }

    /// The level this profile presents, and nothing above it.
    ///
    /// Advertising a feature is a promise that the guest may execute it, and it will do so on
    /// whatever host it lands on. What is promised here is `x86-64-v3` and the host requirement
    /// that goes with it; `AVX-512` is above that line and stays absent.
    #[test]
    fn nothing_above_the_presented_level() {
        let ecx = identify(1, 0)[2];
        assert_ne!(ecx & (1 << 28), 0, "AVX, which the level includes");
        assert_ne!(identify(7, 0)[1] & (1 << 5), 0, "AVX2, likewise");
        assert_eq!(identify(7, 0)[2] & (1 << 11), 0, "AVX-512 VNNI, which it does not");
        assert_eq!(identify(7, 0)[1] & (1 << 16), 0, "AVX-512F, likewise");
    }

    /// A leaf nobody modelled answers zeros rather than another leaf's contents.
    #[test]
    fn an_unmodelled_leaf_is_empty() {
        assert_eq!(identify(0x0000_0016, 0), [0, 0, 0, 0]);
        assert_eq!(identify(0x8000_001D, 0), [0, 0, 0, 0]);
        assert_ne!(identify(0x0000_0001, 0), [0, 0, 0, 0], "and a modelled one is not");
    }

    /// The brand is forty-eight bytes and ends where it says it does.
    #[test]
    fn the_brand_reads_back_whole() {
        let mut spelled = Vec::new();
        for leaf in 0x8000_0002..=0x8000_0004u32 {
            for word in identify(leaf, 0) {
                spelled.extend_from_slice(&word.to_le_bytes());
            }
        }
        assert_eq!(spelled, BRAND);
        assert!(String::from_utf8_lossy(&spelled).starts_with("bohr virtual"));
    }

    /// **The whole table, as one number.**
    ///
    /// A guest branches on every value here, so a profile that moved would make a stored bundle
    /// describe a machine that no longer exists and a replay diverge for a reason nobody would
    /// think to look for. Changing this digest is a decision, and this is where it is taken.
    #[test]
    fn the_profile_is_the_one_that_was_committed() {
        let leaves = [
            0x0000_0000,
            0x0000_0001,
            0x0000_0007,
            0x0000_000D,
            0x8000_0000,
            0x8000_0001,
            0x8000_0002,
            0x8000_0003,
            0x8000_0004,
            0x8000_0006,
            0x8000_0008,
        ];
        let mut bytes = Vec::new();
        for leaf in leaves {
            for word in identify(leaf, 0) {
                bytes.extend_from_slice(&word.to_le_bytes());
            }
        }
        assert_eq!(
            bohr_types::Digest::of(&bytes).to_string(),
            "blake3:8dad29de439c78a6168b76cc082c88405bf0a7f00309cf4ce4e2803d6738b948",
            "the presented processor changed; see the note on this test"
        );
    }
}
