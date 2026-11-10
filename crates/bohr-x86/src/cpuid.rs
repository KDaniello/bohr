//! `cpuid`, the instruction that asks the processor what it is.
//!
//! A guest branches on the answer: which copy routine to use, whether hardware randomness exists,
//! how many threads to start. Answered by the real processor, a bundle recorded on one machine
//! would stop replaying on another (goal T4).
//!
//! `arch_prctl(ARCH_SET_CPUID)` makes the instruction fault, but only where the hardware and any
//! hypervisor allow it. Elsewhere this module finds the instruction in the object and replaces it
//! with one that faults.
//!
//! The answer itself comes from the machine model.

use super::sites::{self, Site};

/// The instruction: `cpuid`, two bytes and no operands.
pub const CPUID: [u8; 2] = [0x0F, 0xA2];

/// What replaces it: `hlt`, then a byte that never runs.
///
/// Two bytes for two, so no address moves. `hlt` is privileged, so user mode faults on it at once.
///
/// Compilers emit `ud2` on purpose but never `hlt` in user code, so a `hlt` fault is recognised by
/// the byte at the fault address alone. That matters because the kernel maps the program, and the
/// supervisor has no table of rewritten offsets to look up.
pub const TRAP: [u8; 2] = [0xF4, 0x90];

/// Every place in an object where the instruction asks the processor what it is.
#[must_use]
pub fn sites(object: &[u8]) -> Vec<Site> {
    let mut candidates = Vec::new();
    for (from, to) in sites::executable(object) {
        for (offset, window) in object[from..to].windows(2).enumerate() {
            if window == CPUID {
                candidates.push(Site { at: from + offset, len: 2, confirmed: false });
            }
        }
    }
    sites::confirm(object, candidates)
}

/// A copy of the object with every established site replaced, and where each of them was.
///
/// The offsets tell a fault on a replaced instruction from one of the guest's own.
#[must_use]
pub fn trapped(object: &[u8]) -> Option<(Vec<u8>, Vec<usize>)> {
    let found: Vec<usize> = sites(object)
        .into_iter()
        .filter(|site| site.confirmed && site.len == 2)
        .map(|site| site.at)
        .collect();
    if found.is_empty() {
        return None;
    }
    let mut out = object.to_vec();
    for at in &found {
        out[*at..*at + 2].copy_from_slice(&TRAP);
    }
    Some((out, found))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instruction is replaced in place; nothing after it moves.
    #[test]
    fn the_instruction_is_replaced_in_the_space_it_occupied() {
        let mut object = Vec::new();
        for _ in 0..12 {
            // xor eax, eax ; nop ; nop -- nothing here can hide the pair
            object.extend_from_slice(&[0x31, 0xC0, 0x90, 0x90]);
        }
        let at = object.len();
        object.extend_from_slice(&CPUID);
        object.extend_from_slice(&[0x90; 8]);

        let (patched, found) = trapped(&object).expect("one site");
        assert_eq!(found, vec![at]);
        assert_eq!(&patched[at..at + 2], &TRAP);
        assert_eq!(patched.len(), object.len(), "and nothing after it moved");
    }

    /// The same pair inside a longer instruction is neither reported as established nor replaced.
    #[test]
    fn a_pair_inside_an_instruction_is_left_alone() {
        let mut object = Vec::new();
        for _ in 0..12 {
            // mov rax, [rip + 0x0011a20f] -- bytes three and four spell the instruction
            object.extend_from_slice(&[0x48, 0x8B, 0x05, 0x0F, 0xA2, 0x11, 0x00]);
        }
        assert!(sites(&object).iter().all(|site| !site.confirmed));
        assert!(trapped(&object).is_none());
    }

    /// An object without the instruction gives no copy.
    #[test]
    fn an_object_that_never_asks_is_not_copied() {
        assert!(trapped(&[0x90; 64]).is_none());
    }
}
