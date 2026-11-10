//! The instructions that take a number from the processor, and what to put in their place.
//!
//! `RDRAND` and `RDSEED` cannot be trapped: no control register bit or `prctl` flag makes them
//! fault. This module finds them in the object and replaces them.
//!
//! The replacement has the same length, so nothing moves: `rdrand r32` is three bytes, as is
//! `xor r, r` plus `stc`; the wide forms keep their `REX` byte. The guest gets zero with the
//! success flag set, which callers must handle since the real instructions may fail.
//!
//! Zero is weaker than a draw from the tape: two call sites get the same number, and a guest that
//! waits for two distinct values would spin. The run repeats, which is what matters here.
//!
//! Code generated at run time, such as a JIT's, is out of reach of the scan.

use super::sites::{self, Site};

/// A copy of the object with every site replaced, and the sites that were replaced.
///
/// `None` where nothing was replaced, so a caller need not compare the copies.
#[must_use]
pub fn without_hardware_randomness(object: &[u8]) -> Option<(Vec<u8>, Vec<Site>)> {
    let found = sites(object);
    if found.is_empty() {
        return None;
    }
    let mut out = object.to_vec();
    let mut done = Vec::new();
    // Only confirmed sites. An unconfirmed one may be the middle of another instruction, and
    // overwriting it would silently corrupt the program.
    for site in found.into_iter().filter(|site| site.confirmed) {
        if replace(&mut out, site) {
            done.push(site);
        }
    }
    (!done.is_empty()).then_some((out, done))
}

/// Writes the replacement over one site, if its shape is a known one.
///
/// The replacement clears the register named in the low three bits of the modrm byte.
fn replace(object: &mut [u8], site: Site) -> bool {
    let Some(bytes) = object.get_mut(site.at..site.at + site.len) else {
        return false;
    };
    // `0F C7 /6` and `/7` with the top two bits set, optionally behind one `REX`.
    let (rex, body) = match site.len {
        3 => (None, &bytes[..3]),
        4 if bytes[0] & 0xF0 == 0x40 => (Some(bytes[0]), &bytes[1..4]),
        _ => return false,
    };
    if body[0] != 0x0F || body[1] != 0xC7 {
        return false;
    }
    // The register the instruction would have written, extended by `REX.B` where there is one.
    let reg = (body[2] & 0b111) | (u8::from(rex.is_some_and(|byte| byte & 0x01 != 0)) << 3);

    // `xor r, r` (`31 /r`) clears the register and `stc` reports success, in the same space.
    let modrm = 0xC0 | ((reg & 0b111) << 3) | (reg & 0b111);
    match site.len {
        3 => bytes.copy_from_slice(&[0x31, modrm, 0xF9]),
        // The wide form keeps a `REX` to clear the whole register; `REX.R` and `REX.B` match
        // because both operands are the same register.
        _ => {
            let keep = 0x40 | (u8::from(reg >= 8) * 0x05);
            bytes.copy_from_slice(&[keep, 0x31, modrm, 0xF9]);
        }
    }
    true
}

/// Every place in an object where an instruction takes a number from the processor.
///
/// The byte scan finds every real site and some false ones; confirmation tells them apart.
#[must_use]
pub fn sites(object: &[u8]) -> Vec<Site> {
    let mut candidates = Vec::new();
    for (from, to) in sites::executable(object) {
        scan_range(object, from, to, &mut candidates);
    }
    sites::confirm(object, candidates)
}

/// The byte-pair search over one range.
fn scan_range(object: &[u8], from: usize, to: usize, out: &mut Vec<Site>) {
    for (offset, window) in object[from..to].windows(3).enumerate() {
        let i = from + offset;
        if window[0] != 0x0F || window[1] != 0xC7 {
            continue;
        }
        let reg = (window[2] >> 3) & 0b111;
        // `/6` is `RDRAND` and `/7` is `RDSEED`.
        if (reg == 6 || reg == 7) && window[2] >= 0xC0 {
            // Reported at the opcode. Whether a `REX` byte before it belongs to it is settled by
            // confirmation, which also extends the site to four bytes.
            out.push(Site { at: i, len: 3, confirmed: false });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The narrow form is replaced in its three bytes.
    #[test]
    fn a_thirty_two_bit_read_becomes_a_cleared_register() {
        // rdrand eax
        let mut object = vec![0x0F, 0xC7, 0xF0];
        assert!(replace(&mut object, Site { at: 0, len: 3, confirmed: true }));
        assert_eq!(object, vec![0x31, 0xC0, 0xF9], "xor eax, eax ; stc");
    }

    /// The replacement clears the register the instruction named.
    #[test]
    fn the_replacement_clears_the_register_that_was_named() {
        // rdrand ecx, rdrand edx, rdseed ebx
        for (encoded, expected) in [
            ([0x0F, 0xC7, 0xF1], [0x31, 0xC9, 0xF9]),
            ([0x0F, 0xC7, 0xF2], [0x31, 0xD2, 0xF9]),
            ([0x0F, 0xC7, 0xFB], [0x31, 0xDB, 0xF9]),
        ] {
            let mut object = encoded.to_vec();
            assert!(replace(&mut object, Site { at: 0, len: 3, confirmed: true }));
            assert_eq!(object, expected, "{encoded:02x?}");
        }
    }

    /// The wide form keeps its prefix and its length.
    #[test]
    fn a_sixty_four_bit_read_is_replaced_in_four_bytes() {
        // rdrand rax
        let mut object = vec![0x48, 0x0F, 0xC7, 0xF0, 0xC3];
        assert!(replace(&mut object, Site { at: 0, len: 4, confirmed: true }));
        assert_eq!(object[..4], [0x40, 0x31, 0xC0, 0xF9]);
        assert_eq!(object[4], 0xC3, "and nothing after it moved");
    }

    /// Bytes that are not one of these instructions are left unchanged.
    #[test]
    fn nothing_else_is_written_over() {
        let mut object = vec![0x48, 0x8B, 0x05, 0x00];
        assert!(!replace(&mut object, Site { at: 0, len: 4, confirmed: true }));
        assert_eq!(object, vec![0x48, 0x8B, 0x05, 0x00]);
    }

    /// An object without the instructions gives no copy.
    #[test]
    fn an_object_without_one_is_not_copied() {
        assert!(without_hardware_randomness(&[0x90; 64]).is_none());
    }

    /// A byte pair inside a longer instruction is not confirmed or rewritten.
    #[test]
    fn a_pair_inside_an_instruction_is_not_a_site() {
        // Instructions whose displacement contains the pair, repeated so several sweeps step over
        // the same byte.
        let mut object = Vec::new();
        for _ in 0..12 {
            // mov rax, [rip + 0x0011c70f] -- seven bytes, and bytes three and four are the pair
            object.extend_from_slice(&[0x48, 0x8B, 0x05, 0x0F, 0xC7, 0x11, 0x00]);
        }
        let found = sites(&object);
        assert!(
            found.iter().all(|site| !site.confirmed),
            "nothing here may be rewritten: {found:?}"
        );
        assert!(without_hardware_randomness(&object).is_none());
    }

    /// A real instruction among ordinary ones is confirmed and rewritten.
    ///
    /// Together with the test above, this rules out both confirming nothing and confirming all.
    #[test]
    fn a_real_one_between_ordinary_instructions_is_rewritten() {
        let mut object = Vec::new();
        for _ in 0..12 {
            // xor eax, eax ; nop ; nop -- four bytes, nothing that could hide a pair
            object.extend_from_slice(&[0x31, 0xC0, 0x90, 0x90]);
        }
        let at = object.len();
        object.extend_from_slice(&[0x0F, 0xC7, 0xF0]);
        object.extend_from_slice(&[0x90; 8]);

        let found = sites(&object);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].confirmed, "several sweeps from behind it must agree: {found:?}");
        assert_eq!(found[0].at, at);

        let (patched, done) = without_hardware_randomness(&object).expect("one site to rewrite");
        assert_eq!(done.len(), 1);
        assert_eq!(&patched[at..at + 3], &[0x31, 0xC0, 0xF9]);
        assert_eq!(patched.len(), object.len(), "and the object is the length it was");
    }

    /// Without symbols nothing can be confirmed, and the candidate is still reported.
    #[test]
    fn without_anchors_every_candidate_stands() {
        // A displacement that spells the pair, with no symbol table to measure against.
        let object = [0x48, 0x8B, 0x05, 0x0F, 0xC7, 0xF0, 0x11, 0xC3];
        assert_eq!(sites(&object).len(), 1, "an unconfirmed candidate is still reported");
    }
}
