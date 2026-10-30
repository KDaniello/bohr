//! Where the functions are, and which bytes inside one begin an instruction.
//!
//! A byte pair is not an instruction because it looks like one. `0F A2` is `cpuid` where an
//! instruction begins and is two bytes of a displacement where it does not, and the two cases are
//! told apart only by walking the code and measuring every step ([`crate::code::insn`]).
//!
//! **The walk is anchored, not started from the section.** Text carries things that are not code:
//! strings sitting beside hand-written assembly, alignment padding, jump tables. A sweep that runs
//! from the first byte of `.text` eventually walks into one of them, comes out of it on the wrong
//! boundary, and every measurement after that is wrong without saying so. Anchoring on a function
//! symbol and stopping at its declared size bounds the damage to the one function: a wrong step
//! inside it cannot escape into the next, and the regions between functions are never walked at
//! all.
//!
//! What this does not do: find anything in a program that carries no symbols. A stripped object
//! has no anchors, and the honest answer there is to say so rather than to sweep blindly — which
//! is what a caller does with an empty result.

use super::insn;

/// One function: where it starts in the file, where it starts in memory, and how long it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Function {
    /// Offset of the first byte within the file.
    pub at: usize,
    /// Address the first byte will have once mapped.
    pub address: u64,
    /// Bytes, as the symbol declares them.
    pub len: usize,
}

/// Every function an object names, in the order the symbol table lists them.
///
/// Only those with a size and only those whose bytes are in the file: a symbol declaring a length
/// that runs past the end describes an object this build cannot read, and guessing where it really
/// ends would be inventing the anchor the whole approach rests on.
pub fn functions(bytes: &[u8]) -> Vec<Function> {
    let Some(sections) = Sections::of(bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for table in [2u32, 11] {
        // `SHT_SYMTAB` first, then `SHT_DYNSYM`: a stripped object keeps the second and loses the
        // first, and an object that has both lists the same functions in each.
        let Some((symbols, _)) = sections.by_kind(table) else {
            continue;
        };
        for entry in symbols.chunks_exact(24) {
            // Type is the low four bits of the info byte; two is a function.
            if entry[4] & 0xF != 2 {
                continue;
            }
            let address = u64::from_le_bytes(entry[8..16].try_into().unwrap_or_default());
            let len = u64::from_le_bytes(entry[16..24].try_into().unwrap_or_default()) as usize;
            if len == 0 || address == 0 {
                continue;
            }
            let Some(at) = sections.file_offset(address) else {
                continue;
            };
            if at + len <= bytes.len() {
                out.push(Function { at, address, len });
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    out
}

/// Every address inside `code` at which an instruction begins.
///
/// Stops at the first byte pair no instruction may start with, because a sweep that guessed there
/// would report boundaries that are not ones. What it has measured up to that point still holds:
/// the guess would only have been about what comes after.
pub fn boundaries(code: &[u8], base: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < code.len() {
        let Some(found) = insn::measure(&code[at..]) else {
            break;
        };
        out.push(base + at as u64);
        at += found.len;
    }
    out
}

/// The section table, and the mapping from an address to the byte in the file that becomes it.
struct Sections<'a> {
    bytes: &'a [u8],
    headers: &'a [u8],
    count: usize,
}

impl<'a> Sections<'a> {
    /// Reads the table, or nothing where the object has none this build can walk.
    fn of(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 {
            return None;
        }
        let off = u64::from_le_bytes(bytes.get(0x28..0x30)?.try_into().ok()?) as usize;
        let size = u16::from_le_bytes(bytes.get(0x3A..0x3C)?.try_into().ok()?) as usize;
        let count = u16::from_le_bytes(bytes.get(0x3C..0x3E)?.try_into().ok()?) as usize;
        if size != 64 || count == 0 {
            return None;
        }
        let headers = bytes.get(off..off + size * count)?;
        Some(Self { bytes, headers, count })
    }

    /// One header, as its sixty-four bytes.
    fn header(&self, index: usize) -> &'a [u8] {
        &self.headers[index * 64..(index + 1) * 64]
    }

    /// The contents of the first section of this kind, and the index it was found at.
    fn by_kind(&self, kind: u32) -> Option<(&'a [u8], usize)> {
        (0..self.count).find_map(|index| {
            let header = self.header(index);
            let found = u32::from_le_bytes(header[4..8].try_into().ok()?);
            if found != kind {
                return None;
            }
            let off = u64::from_le_bytes(header[0x18..0x20].try_into().ok()?) as usize;
            let size = u64::from_le_bytes(header[0x20..0x28].try_into().ok()?) as usize;
            self.bytes.get(off..off + size).map(|body| (body, index))
        })
    }

    /// Which byte of the file becomes this address once the object is mapped.
    ///
    /// Read from the sections rather than from the program headers because a symbol names an
    /// address inside one section, and a section carries both numbers. Sections that occupy no
    /// space in the file are skipped: the zero-filled ones map to nothing to walk.
    fn file_offset(&self, address: u64) -> Option<usize> {
        (0..self.count).find_map(|index| {
            let header = self.header(index);
            let kind = u32::from_le_bytes(header[4..8].try_into().ok()?);
            let flags = u64::from_le_bytes(header[8..16].try_into().ok()?);
            // Allocated and not zero-filled: `SHT_NOBITS` is the second.
            if flags & 0x2 == 0 || kind == 8 {
                return None;
            }
            let start = u64::from_le_bytes(header[0x10..0x18].try_into().ok()?);
            let off = u64::from_le_bytes(header[0x18..0x20].try_into().ok()?) as usize;
            let size = u64::from_le_bytes(header[0x20..0x28].try_into().ok()?);
            (address >= start && address < start + size).then(|| off + (address - start) as usize)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guest built by `make guests`, or nothing where it has not been built.
    fn guest(name: &str) -> Option<Vec<u8>> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/guests/build")
            .join(name);
        std::fs::read(path).ok()
    }

    /// Boundaries are measured, not searched for.
    #[test]
    fn a_sweep_lands_on_every_instruction_and_no_other_byte() {
        // xor eax, eax; mov edi, 1; syscall; ret
        let code = [0x31, 0xC0, 0xBF, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x05, 0xC3];
        assert_eq!(boundaries(&code, 0x1000), vec![0x1000, 0x1002, 0x1007, 0x1009]);
    }

    /// A byte pair inside a longer instruction is not a boundary, and this is the entire reason
    /// the sweep exists.
    ///
    /// The displacement of an ordinary load spells `0F A2`. A search finds `cpuid` there; a sweep
    /// finds one instruction seven bytes long and no boundary inside it.
    #[test]
    fn a_pair_inside_another_instruction_is_not_one() {
        // mov rax, [rip + 0x1100a20f] -- whose displacement carries the bytes of `cpuid`
        let code = [0x48, 0x8B, 0x05, 0x0F, 0xA2, 0x00, 0x11, 0xC3];
        let found = boundaries(&code, 0);
        assert_eq!(found, vec![0, 7], "the load, then the return");
        assert!(!found.contains(&3), "and nothing at the byte a search would have named");
    }

    /// The sweep stops where the bytes stop being instructions rather than guessing onward.
    #[test]
    fn a_sweep_stops_rather_than_guesses() {
        // ret, then an opcode no sixty-four bit program may contain
        assert_eq!(boundaries(&[0xC3, 0x06, 0x06], 0), vec![0]);
    }

    /// **Every function of a real object is walked end to end.**
    ///
    /// The anchor and the measurement checking each other: a symbol says a function is `n` bytes
    /// long, and a sweep of it must consume exactly `n` — landing short means an instruction was
    /// measured too long, landing over means one was measured too short, and either way the walk
    /// was not on boundaries. Run over every function a guest carries, which is thousands of them
    /// including everything `libc` brought along.
    #[test]
    fn every_function_measures_to_the_length_its_symbol_declares() {
        let Some(bytes) = guest("two_threads") else {
            return;
        };
        let found = functions(&bytes);
        assert!(found.len() > 100, "a static guest carries its whole library: {}", found.len());

        let mut walked = 0;
        let mut exact = 0;
        for function in &found {
            let code = &bytes[function.at..function.at + function.len];
            let mut at = 0;
            while at < code.len() {
                let Some(step) = insn::measure(&code[at..]) else {
                    break;
                };
                at += step.len;
            }
            walked += 1;
            exact += usize::from(at == code.len());
        }
        // Measured at every one of them on the corpus this repository builds. The floor is lower
        // than the measurement because not every symbol has to be code -- some name a table, and
        // hand-written assembly puts constants inside a function's declared span -- and a
        // different compiler is allowed to produce one of those without that being a defect here.
        let ratio = exact * 100 / walked;
        assert!(ratio >= 97, "{exact} of {walked} functions swept exactly ({ratio}%)");
    }
}
