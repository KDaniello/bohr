//! The instructions that take a number from the processor, and what to put in their place.
//!
//! `RDRAND` and `RDSEED` are the one source of nondeterminism the processor offers no way to
//! trap: there is no control register bit and no `prctl` flag that makes either fault, so a
//! supervisor below a hypervisor never learns one was executed. What is left is to see that it is
//! not executed — to find it in the object and put something else there.
//!
//! **Same length for same length, so nothing moves.** `rdrand r32` is three bytes and so is
//! `xor r, r` followed by `stc`; the wide forms carry a `REX` byte and so does the replacement.
//! No relocation, no layout change, no address that used to be one thing and is now another. The
//! guest is handed a zero and told the read succeeded, which every caller of these instructions
//! is required to handle, because the real ones are allowed to fail.
//!
//! **A zero is a weaker answer than the tape would give**, and it is the strongest one available
//! without a trap. Two call sites asking for entropy get the same number, where a draw would give
//! them different ones; a guest that loops until it sees two distinct values would spin. What it
//! buys is that the run repeats, which is the property everything else here rests on.
//!
//! What this does not do: touch code that is not in the object. A program that writes an
//! instruction into a page at run time — a compiler of its own — carries this past anything a scan
//! can reach, and the honest answer there is the hypervisor.

use super::sites::{self, Site};

/// A copy of the object with every site replaced, and the sites that were replaced.
///
/// Nothing where there is nothing to do, so a caller can tell "left alone" from "rewritten and
/// happened to be identical" without comparing megabytes.
#[must_use]
pub fn without_hardware_randomness(object: &[u8]) -> Option<(Vec<u8>, Vec<Site>)> {
    let found = sites(object);
    if found.is_empty() {
        return None;
    }
    let mut out = object.to_vec();
    let mut done = Vec::new();
    // Only what a sweep measured. An unconfirmed site is reported by `sites` and is not rewritten
    // here: writing over bytes that were the middle of another instruction damages that
    // instruction, and a library damaged this way says nothing at all about it.
    for site in found.into_iter().filter(|site| site.confirmed) {
        if replace(&mut out, site) {
            done.push(site);
        }
    }
    (!done.is_empty()).then_some((out, done))
}

/// Writes the replacement over one site, where its shape is one this knows how to replace.
///
/// The register is named in the low three bits of the modrm byte, and the replacement clears that
/// same register — anything else would leave the caller reading a register nobody wrote.
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

    // `xor r, r` is `31 /r` with both operands the same register, and `stc` is one byte. Together
    // they clear the register and say the read worked, in exactly the space that was there.
    let modrm = 0xC0 | ((reg & 0b111) << 3) | (reg & 0b111);
    match site.len {
        3 => bytes.copy_from_slice(&[0x31, modrm, 0xF9]),
        // The wide form keeps a `REX` so the clear is of the whole register, and keeps `REX.R` and
        // `REX.B` together because the two operands of the clear are one register.
        _ => {
            let keep = 0x40 | (u8::from(reg >= 8) * 0x05);
            bytes.copy_from_slice(&[keep, 0x31, modrm, 0xF9]);
        }
    }
    true
}

/// Every place in an object where an instruction takes a number from the processor.
///
/// The search is deliberately generous — it names every real one and some bytes besides — and the
/// confirmation is what tells them apart. Separating the two is what lets the generous half stay
/// simple and the strict half stay honest.
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
        // `/6` is `RDRAND` and `/7` is `RDSEED`; the rest of the group is something else entirely.
        if (reg == 6 || reg == 7) && window[2] >= 0xC0 {
            // A `REX` byte immediately before belongs to this instruction, and the site starts
            // there: replacing three bytes of a four-byte instruction leaves the prefix behind.
            // Reported at the opcode, whatever precedes it: whether a `REX` byte belongs to this
            // instruction is a question for the measurement, not for a look at the byte before.
            out.push(Site { at: i, len: 3, confirmed: false });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The narrow form, replaced in the three bytes it occupies.
    #[test]
    fn a_thirty_two_bit_read_becomes_a_cleared_register() {
        // rdrand eax
        let mut object = vec![0x0F, 0xC7, 0xF0];
        assert!(replace(&mut object, Site { at: 0, len: 3, confirmed: true }));
        assert_eq!(object, vec![0x31, 0xC0, 0xF9], "xor eax, eax ; stc");
    }

    /// The register the caller will read is the register the replacement clears.
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

    /// The wide form keeps its prefix, and the replacement is as long as what it replaces.
    #[test]
    fn a_sixty_four_bit_read_is_replaced_in_four_bytes() {
        // rdrand rax
        let mut object = vec![0x48, 0x0F, 0xC7, 0xF0, 0xC3];
        assert!(replace(&mut object, Site { at: 0, len: 4, confirmed: true }));
        assert_eq!(object[..4], [0x40, 0x31, 0xC0, 0xF9]);
        assert_eq!(object[4], 0xC3, "and nothing after it moved");
    }

    /// A site that is not one of these instructions is left alone rather than overwritten.
    #[test]
    fn nothing_else_is_written_over() {
        let mut object = vec![0x48, 0x8B, 0x05, 0x00];
        assert!(!replace(&mut object, Site { at: 0, len: 4, confirmed: true }));
        assert_eq!(object, vec![0x48, 0x8B, 0x05, 0x00]);
    }

    /// An object with none of them is left alone entirely, and says so by answering nothing.
    #[test]
    fn an_object_without_one_is_not_copied() {
        assert!(without_hardware_randomness(&[0x90; 64]).is_none());
    }

    /// A byte pair inside a longer instruction is neither reported nor rewritten, where the code
    /// around it could be measured.
    #[test]
    fn a_pair_inside_an_instruction_is_not_a_site() {
        // A run of ordinary instructions whose displacement spells the pair, repeated so that
        // several sweeps from behind it all step over the same byte.
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

    /// A real one in the same shape is both reported and rewritten.
    ///
    /// The pair is the point: a rule that confirmed nothing would pass the test above and be
    /// useless, and a rule that confirmed everything would pass this one and corrupt the other.
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

    /// The scan is generous and the sweep is what makes it precise.
    ///
    /// Here there are no symbols, so nothing is judged and the generous answer stands — which is
    /// the rule: unjudged is reported, not dropped.
    #[test]
    fn without_anchors_every_candidate_stands() {
        // A displacement that spells the pair, with no symbol table to measure against.
        let object = [0x48, 0x8B, 0x05, 0x0F, 0xC7, 0xF0, 0x11, 0xC3];
        assert_eq!(sites(&object).len(), 1, "kept, because nothing established what it is");
    }
}
