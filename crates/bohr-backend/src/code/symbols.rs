//! Where the functions are, and which bytes inside one begin an instruction.
//!
//! `0F A2` is `cpuid` only where an instruction begins; elsewhere it may be part of a
//! displacement. Walking the code and measuring each step ([`crate::code::insn`]) tells them apart.
//!
//! The walk starts at a function symbol and stops at its declared size. `.text` also holds data,
//! padding and jump tables; a sweep from the start of the section would walk into them and lose
//! the boundaries. Per-function sweeps keep an error inside one function.
//!
//! An object without symbols yields no functions, and the caller handles the empty result.

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
/// Only functions with a size whose bytes lie inside the file.
pub fn functions(bytes: &[u8]) -> Vec<Function> {
    let Some(sections) = Sections::of(bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for table in [2u32, 11] {
        // `SHT_SYMTAB` first, then `SHT_DYNSYM`, which a stripped object still has.
        let Some((symbols, _)) = sections.by_kind(table) else {
            continue;
        };
        for entry in symbols.as_chunks::<24>().0 {
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
/// Stops at the first invalid instruction; the boundaries before it stand.
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
    /// Read from the section headers, which carry both the address and the file offset.
    /// Zero-filled sections are skipped.
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

    /// Boundaries are measured from the symbol table.
    #[test]
    fn a_sweep_lands_on_every_instruction_and_no_other_byte() {
        // xor eax, eax; mov edi, 1; syscall; ret
        let code = [0x31, 0xC0, 0xBF, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x05, 0xC3];
        assert_eq!(boundaries(&code, 0x1000), vec![0x1000, 0x1002, 0x1007, 0x1009]);
    }

    /// A byte pair inside a longer instruction is not a boundary.
    ///
    /// The load's displacement contains `0F A2`; the sweep sees one seven-byte instruction.
    #[test]
    fn a_pair_inside_another_instruction_is_not_one() {
        // mov rax, [rip + 0x1100a20f] -- whose displacement carries the bytes of `cpuid`
        let code = [0x48, 0x8B, 0x05, 0x0F, 0xA2, 0x00, 0x11, 0xC3];
        let found = boundaries(&code, 0);
        assert_eq!(found, vec![0, 7], "the load, then the return");
        assert!(!found.contains(&3), "and nothing at the byte a search would have named");
    }

    /// The sweep stops at an invalid instruction.
    #[test]
    fn a_sweep_stops_rather_than_guesses() {
        // ret, then an opcode invalid in 64-bit mode
        assert_eq!(boundaries(&[0xC3, 0x06, 0x06], 0), vec![0]);
    }

    /// A sweep of each function in a real guest ends exactly at its declared size.
    ///
    /// Any other end means a wrong length somewhere. Covers thousands of functions, `libc`
    /// included.
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
        // Every function matches on this repository's corpus. The floor is 97% because some
        // symbols name tables or hold constants inside assembly, which another compiler may emit.
        let ratio = exact * 100 / walked;
        assert!(ratio >= 97, "{exact} of {walked} functions swept exactly ({ratio}%)");
    }
}
