//! The instruction that asks the processor what it is.
//!
//! A guest branches on the answer: which copy routine to use, whether the hardware randomness
//! instruction is there to be taken, how many threads to start. Answered by the real silicon, those
//! branches differ per machine — and a bundle produced on one processor stops replaying on another,
//! which is goal T4 gone.
//!
//! **The processor can be made to fault on it, and not everywhere.** `arch_prctl(ARCH_SET_CPUID)`
//! is a hardware facility, and a hypervisor that does not pass the register through takes it away
//! from every guest above it whatever the silicon underneath supports. Where it is missing, the
//! instruction can still be found in the object and replaced by one that faults for a reason
//! nobody else uses.
//!
//! What this does not do: decide what the answer is. That belongs to whoever is modelling the
//! machine, and lives with the rest of what a guest is shown.

use super::sites::{self, Site};

/// The instruction: `cpuid`, two bytes and no operands.
pub const CPUID: [u8; 2] = [0x0F, 0xA2];

/// What replaces it: `hlt`, then a byte that will never run.
///
/// **Two bytes for two, so nothing moves** and no address becomes another. `hlt` is privileged, so
/// a program in user mode that reaches one faults at once and the byte after it is never executed.
///
/// **Chosen for being unmistakable rather than for being undefined.** `ud2` would have been the
/// obvious pick and is the wrong one: a compiler emits it deliberately, at the end of a function
/// that cannot return and wherever a program promised something unreachable, so a fault on it
/// carries no information about who put it there. Nothing emits `hlt` into a user-mode program.
/// That is what lets the fault be recognised from the byte at the fault address, which in turn is
/// what lets this work on the program itself: a guest is started by the interceptor and mapped by
/// the kernel, so a table of rewritten offsets kept over here would have nothing to translate them
/// against.
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
/// The offsets come back because they are the whole answer: a fault at one of them is the
/// instruction this replaced, and a fault anywhere else is the guest's own. Losing them would leave
/// a supervisor guessing from two bytes that a compiler also writes on purpose.
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

    /// Two bytes for two, and the rest of the object where it was.
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

    /// An object that never asks is left alone entirely, and says so by answering nothing.
    #[test]
    fn an_object_that_never_asks_is_not_copied() {
        assert!(trapped(&[0x90; 64]).is_none());
    }
}
