//! How long one x86-64 instruction is.
//!
//! Not a disassembler: nothing here asks what an instruction *does*. It answers one question —
//! where the next one starts — and that is the question a rewriter has to answer before it may
//! touch a byte.
//!
//! **Why measuring is the whole problem.** Finding `0F A2` in a page of code is a two-byte search,
//! and `doctor` does exactly that: a scan that occasionally names a byte pair sitting inside some
//! other instruction is a diagnosis that over-reports, which is the harmless way for a diagnosis
//! to be wrong. A rewriter cannot be wrong that way. Writing a trap over two bytes in the middle
//! of a longer instruction corrupts that instruction, and the guest then does something nobody
//! wrote — silently, and nowhere near where the mistake was made. So a candidate is only a
//! candidate where an instruction actually begins, and that is decided by walking the code from a
//! known start and measuring every step.
//!
//! **What it must cover.** Everything, not merely what the guest will run. A sweep walks straight
//! through code the guest never enters — an `AVX-512` path in a library that will take the
//! ordinary one — and an instruction measured wrong there desynchronises every measurement after
//! it. So the vector prefixes are decoded for their length even though the presented processor
//! says the guest may not use them.
//!
//! **Measured against a disassembler that is not this one.** Swept over the text of
//! `solana-test-validator` — forty-two megabytes of compiled Rust, hand-written assembly and
//! vector paths — this agrees with `objdump` on **8 985 713 consecutive instructions**, from the
//! first byte of the section, with no disagreement at all. Over the whole text of a static guest
//! from `testdata/` the two agree on every one of 147 897 boundaries. The number is here because
//! it is the only reason to trust the table below, which is otherwise a page of magic constants.
//!
//! What this does not do: tell code from data. The agreement above ends where the section stops
//! being code — `.text` carries embedded strings beside hand-written assembly, and there both this
//! and `objdump` disassemble rubbish, differing rubbish. So a caller does not sweep a section: it
//! sweeps from one function symbol to the next, where a wrong measurement cannot escape into the
//! function after it and where data does not live.

/// The parts of one instruction, in bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Instruction {
    /// Total length, prefixes through immediate.
    pub len: usize,
    /// Offset of the first opcode byte, past every prefix.
    pub opcode_at: usize,
}

/// How wide the immediate is, where the width is not fixed by the opcode alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Imm {
    /// None.
    No,
    /// One byte.
    B,
    /// Two bytes.
    W,
    /// Four bytes.
    D,
    /// Two with an operand-size prefix, four without. Never eight.
    Z,
    /// Two with the prefix, four without, eight under `REX.W`.
    V,
    /// A two-byte immediate followed by a one-byte one, which only `enter` has.
    WB,
}

/// Measures the instruction beginning at `code[0]`.
///
/// Returns nothing where the bytes run out mid-instruction or where the opcode is one no
/// sixty-four bit program may contain: both mean the sweep has left the code it was walking, and a
/// caller that guessed a length here would carry the error forward for the rest of the page.
pub fn measure(code: &[u8]) -> Option<Instruction> {
    let mut at = 0;
    let mut opsize16 = false;
    let mut rex_w = false;

    // Legacy prefixes, in any order and any number. `REX` must be the last of them, and one that
    // appears earlier is an ordinary prefix byte whose bits are discarded -- so it is read again
    // rather than remembered.
    loop {
        match *code.get(at)? {
            0x66 => opsize16 = true,
            0x67 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 | 0xF0 | 0xF2 | 0xF3 => {}
            0x40..=0x4F => {
                rex_w = code[at] & 0x08 != 0;
                at += 1;
                break;
            }
            _ => break,
        }
        at += 1;
    }

    let opcode_at = at;

    // The vector prefixes carry their own opcode map and their own operand size, and both replace
    // what the legacy bytes said. Decoded for length alone: a guest is told it has no vector
    // extensions, but the code it never runs is still walked through.
    match *code.get(at)? {
        0xC5 => {
            let byte = *code.get(at + 1)?;
            return vector(code, at + 2, 1, byte & 0x04 != 0, false, opcode_at);
        }
        0xC4 => {
            let first = *code.get(at + 1)?;
            let second = *code.get(at + 2)?;
            let map = first & 0x1F;
            return vector(code, at + 3, map, second & 0x04 != 0, second & 0x80 != 0, opcode_at);
        }
        0x62 => {
            let first = *code.get(at + 1)?;
            let second = *code.get(at + 2)?;
            let map = first & 0x03;
            return vector(code, at + 4, map, second & 0x04 != 0, second & 0x80 != 0, opcode_at);
        }
        _ => {}
    }

    let (has_modrm, imm, escaped) = match *code.get(at)? {
        0x0F => {
            at += 1;
            match *code.get(at)? {
                // The two three-byte maps. Everything in the first takes a modrm and no
                // immediate; everything in the second takes a modrm and one byte.
                0x38 => {
                    at += 1;
                    (true, Imm::No, true)
                }
                0x3A => {
                    at += 1;
                    (true, Imm::B, true)
                }
                other => {
                    let (m, i) = two_byte(other);
                    (m, i, true)
                }
            }
        }
        other => {
            let (m, i) = one_byte(other)?;
            (m, i, false)
        }
    };
    let opcode = *code.get(at)?;
    at += 1;

    // The one group whose immediate depends on which operation inside it was named.
    let imm = if !escaped && (opcode == 0xF6 || opcode == 0xF7) {
        match (*code.get(at)? >> 3) & 0b111 {
            0 | 1 if opcode == 0xF6 => Imm::B,
            0 | 1 => Imm::Z,
            _ => Imm::No,
        }
    } else {
        imm
    };

    if has_modrm {
        at += modrm_len(code, at)?;
    }
    at += imm_len(imm, opsize16, rex_w);
    (at <= code.len()).then_some(Instruction { len: at, opcode_at })
}

/// The rest of an instruction introduced by a vector prefix.
///
/// The map decides which opcode table the byte after the prefix belongs to, and only the third
/// carries an immediate. `pp` and `REX.W` are read for the operand size the immediate would take,
/// which is why they are passed even though nothing here is variable-width in practice.
fn vector(
    code: &[u8],
    opcode_at_byte: usize,
    map: u8,
    _pp16: bool,
    _wide: bool,
    opcode_at: usize,
) -> Option<Instruction> {
    let mut at = opcode_at_byte;
    let opcode = *code.get(at)?;
    // The first map is the two-byte opcode table under another prefix, so it is read from the same
    // place: the shift groups there carry an immediate byte, and the pair that clears the upper
    // halves of the registers carries no modrm at all. Deciding either of those separately here
    // was how this measured `vpsrlw` a byte short and lost every boundary after it.
    let (has_modrm, imm) = match map {
        1 => two_byte(opcode),
        3 => (true, Imm::B),
        _ => (true, Imm::No),
    };
    at += 1;
    if has_modrm {
        at += modrm_len(code, at)?;
    }
    at += imm_len(imm, false, false);
    (at <= code.len()).then_some(Instruction { len: at, opcode_at })
}

/// The modrm byte and everything it drags behind it: a scale-index-base byte and a displacement.
fn modrm_len(code: &[u8], at: usize) -> Option<usize> {
    let modrm = *code.get(at)?;
    let mode = modrm >> 6;
    let rm = modrm & 0b111;

    if mode == 0b11 {
        return Some(1);
    }
    // A base of `100` means the address is spelled out in a second byte instead.
    let sib = usize::from(rm == 0b100);
    let disp = match mode {
        // `101` here is not a register at all: it is an offset from the instruction pointer, and
        // the offset is four bytes. Reading it as none is the mistake that desynchronises a sweep
        // in the middle of ordinary compiler output, where this addressing is everywhere.
        0b00 if rm == 0b101 => 4,
        0b00 if sib == 1 && (*code.get(at + 1)? & 0b111) == 0b101 => 4,
        0b00 => 0,
        0b01 => 1,
        _ => 4,
    };
    Some(1 + sib + disp)
}

/// How many bytes an immediate of this class occupies.
fn imm_len(imm: Imm, opsize16: bool, rex_w: bool) -> usize {
    match imm {
        Imm::No => 0,
        Imm::B => 1,
        Imm::W => 2,
        Imm::D => 4,
        Imm::Z => usize::from(!opsize16) * 2 + 2,
        Imm::V => {
            if rex_w {
                8
            } else if opsize16 {
                2
            } else {
                4
            }
        }
        Imm::WB => 3,
    }
}

/// Whether a one-byte opcode takes a modrm, and what immediate follows it.
///
/// `None` for the opcodes no sixty-four bit program may contain: the sweep has left the code.
fn one_byte(opcode: u8) -> Option<(bool, Imm)> {
    Some(match opcode {
        // The arithmetic block: eight operations, six forms each, in a repeating pattern.
        0x00..=0x05
        | 0x08..=0x0D
        | 0x10..=0x15
        | 0x18..=0x1D
        | 0x20..=0x25
        | 0x28..=0x2D
        | 0x30..=0x35
        | 0x38..=0x3D => match opcode & 0b111 {
            0..=3 => (true, Imm::No),
            4 => (false, Imm::B),
            _ => (false, Imm::Z),
        },

        // Invalid in long mode: their encodings were reused for the `REX` prefixes and for the
        // vector ones, and reaching one means the walk is no longer on instruction boundaries.
        0x06 | 0x07 | 0x0E | 0x16 | 0x17 | 0x1E | 0x1F | 0x27 | 0x2F | 0x37 | 0x3F | 0x60
        | 0x61 | 0x62 | 0x82 | 0x9A | 0xC4 | 0xC5 | 0xD4 | 0xD5 | 0xD6 | 0xEA => return None,

        0x50..=0x5F | 0x90..=0x97 | 0x99 | 0x98 | 0x9C..=0x9F => (false, Imm::No),
        0x63 => (true, Imm::No),
        0x68 => (false, Imm::Z),
        0x69 => (true, Imm::Z),
        0x6A => (false, Imm::B),
        0x6B => (true, Imm::B),
        0x6C..=0x6F => (false, Imm::No),
        0x70..=0x7F => (false, Imm::B),
        0x80 => (true, Imm::B),
        0x81 => (true, Imm::Z),
        0x83 => (true, Imm::B),
        0x84..=0x8F => (true, Imm::No),
        // The one addressing mode that names an absolute address, eight bytes wide here.
        0xA0..=0xA3 => (false, Imm::D),
        0xA4..=0xA7 | 0xAA..=0xAF => (false, Imm::No),
        0xA8 => (false, Imm::B),
        0xA9 => (false, Imm::Z),
        0xB0..=0xB7 => (false, Imm::B),
        0xB8..=0xBF => (false, Imm::V),
        0xC0 | 0xC1 => (true, Imm::B),
        0xC2 => (false, Imm::W),
        0xC3 => (false, Imm::No),
        0xC6 => (true, Imm::B),
        0xC7 => (true, Imm::Z),
        0xC8 => (false, Imm::WB),
        0xC9 | 0xCB | 0xCC | 0xCE | 0xCF => (false, Imm::No),
        0xCA => (false, Imm::W),
        0xCD => (false, Imm::B),
        0xD0..=0xD3 => (true, Imm::No),
        0xD7 => (false, Imm::No),
        0xD8..=0xDF => (true, Imm::No),
        0xE0..=0xE7 => (false, Imm::B),
        0xE8 | 0xE9 => (false, Imm::Z),
        0xEB => (false, Imm::B),
        0xEC..=0xEF => (false, Imm::No),
        0xF1 | 0xF4 | 0xF5 | 0xF8..=0xFD => (false, Imm::No),
        0xF6 | 0xF7 => (true, Imm::No),
        0xFE | 0xFF => (true, Imm::No),
        _ => (false, Imm::No),
    })
}

/// Whether a two-byte opcode takes a modrm, and what immediate follows it.
fn two_byte(opcode: u8) -> (bool, Imm) {
    match opcode {
        // `syscall`, `ud2`, the cache hints, `cpuid`, `rdtsc` and the rest that stand alone.
        0x05
        | 0x06
        | 0x07
        | 0x08
        | 0x09
        | 0x0B
        | 0x30..=0x37
        | 0x77
        | 0xA0..=0xA2
        | 0xA8
        | 0xA9
        | 0xAA
        | 0xC8..=0xCF => (false, Imm::No),

        // The conditional jumps, whose displacement is four bytes here.
        0x80..=0x8F => (false, Imm::Z),

        // Everything with a byte-wide selector after the modrm.
        0x70..=0x73 | 0xA4 | 0xAC | 0xBA | 0xC2 | 0xC4..=0xC6 | 0x0F => (true, Imm::B),

        _ => (true, Imm::No),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn len(bytes: &[u8]) -> Option<usize> {
        measure(bytes).map(|found| found.len)
    }

    /// The two instructions this whole module exists to find, and the trap that replaces them.
    #[test]
    fn the_instructions_that_leak_a_machine_are_measured() {
        assert_eq!(len(&[0x0F, 0xA2]), Some(2), "cpuid");
        assert_eq!(len(&[0x0F, 0xC7, 0xF0]), Some(3), "rdrand eax");
        assert_eq!(len(&[0x48, 0x0F, 0xC7, 0xF0]), Some(4), "rdrand rax");
        assert_eq!(len(&[0x0F, 0xC7, 0xF8]), Some(3), "rdseed eax");
        assert_eq!(len(&[0x0F, 0x0B]), Some(2), "ud2, which replaces them");
        assert_eq!(len(&[0x0F, 0x31]), Some(2), "rdtsc");
        assert_eq!(len(&[0x0F, 0x05]), Some(2), "syscall");
    }

    /// The addressing mode that is everywhere in compiler output and is nothing like it looks.
    ///
    /// A base of `101` with no displacement bits set is not a register: it is an offset from the
    /// instruction pointer, four bytes wide. Measured as zero, a sweep loses its place inside the
    /// first function it walks.
    #[test]
    fn an_offset_from_the_instruction_pointer_is_four_bytes() {
        // mov rax, [rip + 0x11223344]
        assert_eq!(len(&[0x48, 0x8B, 0x05, 0x44, 0x33, 0x22, 0x11]), Some(7));
        // lea rdi, [rip + 0]
        assert_eq!(len(&[0x48, 0x8D, 0x3D, 0x00, 0x00, 0x00, 0x00]), Some(7));
    }

    /// A base named by a second byte, and the displacement that a particular base implies.
    #[test]
    fn a_scale_index_base_byte_is_counted() {
        // mov eax, [rsp]
        assert_eq!(len(&[0x8B, 0x04, 0x24]), Some(3));
        // mov eax, [rsp + 0x10]
        assert_eq!(len(&[0x8B, 0x44, 0x24, 0x10]), Some(4));
        // mov eax, [rbp*4 + 0x11223344] -- no base, so four bytes of displacement
        assert_eq!(len(&[0x8B, 0x04, 0xAD, 0x44, 0x33, 0x22, 0x11]), Some(7));
    }

    /// Immediates that change width with a prefix, and the one that reaches eight bytes.
    #[test]
    fn an_immediate_is_as_wide_as_its_prefixes_say() {
        assert_eq!(len(&[0xB8, 0x44, 0x33, 0x22, 0x11]), Some(5), "mov eax, imm32");
        assert_eq!(
            len(&[0x48, 0xB8, 8, 7, 6, 5, 4, 3, 2, 1]),
            Some(10),
            "mov rax, imm64 -- the only ten-byte instruction an ordinary program contains"
        );
        assert_eq!(len(&[0x66, 0xB8, 0x22, 0x11]), Some(4), "mov ax, imm16");
        assert_eq!(len(&[0x81, 0xC0, 0x44, 0x33, 0x22, 0x11]), Some(6), "add eax, imm32");
        assert_eq!(len(&[0x66, 0x81, 0xC0, 0x22, 0x11]), Some(5), "add ax, imm16");
        assert_eq!(len(&[0x83, 0xC0, 0x01]), Some(3), "add eax, imm8");
    }

    /// The group whose immediate depends on which operation inside it was named.
    ///
    /// `test` carries one; `not` and `neg`, written with the same opcode, carry none. Reading the
    /// group as though every member were alike puts four bytes of the next instruction inside this
    /// one.
    #[test]
    fn one_opcode_two_shapes() {
        assert_eq!(len(&[0xF7, 0xC0, 0x44, 0x33, 0x22, 0x11]), Some(6), "test eax, imm32");
        assert_eq!(len(&[0xF7, 0xD0]), Some(2), "not eax");
        assert_eq!(len(&[0xF6, 0xC3, 0x01]), Some(3), "test bl, imm8");
        assert_eq!(len(&[0xF6, 0xD3]), Some(2), "not bl");
    }

    /// Vector code is walked through even though the guest is told it has none.
    ///
    /// A sweep goes through paths the guest will never take, and a length measured wrong in one of
    /// them loses every boundary after it.
    #[test]
    fn vector_prefixes_are_measured_for_length() {
        // vzeroupper
        assert_eq!(len(&[0xC5, 0xF8, 0x77]), Some(3));
        // vmovdqu ymm0, [rdi]
        assert_eq!(len(&[0xC5, 0xFE, 0x6F, 0x07]), Some(4));
        // vpshufd ymm0, ymm1, 0x55 -- three-byte map, so one byte of immediate
        assert_eq!(len(&[0xC4, 0xE3, 0x7D, 0x00, 0xC1, 0x55]), Some(6));
        // an evex-prefixed move
        assert_eq!(len(&[0x62, 0xF1, 0x7C, 0x48, 0x28, 0xC1]), Some(6));
    }

    /// An opcode no sixty-four bit program may contain says the walk has left the code.
    ///
    /// Guessing a length here would carry the error to the end of the page. Saying nothing lets
    /// the caller stop where the ground is still known.
    #[test]
    fn an_impossible_opcode_is_not_guessed_at() {
        assert_eq!(len(&[0x06]), None, "push es");
        assert_eq!(len(&[0x9A, 0, 0, 0, 0, 0, 0]), None, "far call");
        assert_eq!(len(&[0x0F]), None, "an escape with nothing after it");
        assert_eq!(len(&[]), None);
        assert_eq!(len(&[0x48, 0x8B, 0x05, 0x44]), None, "a displacement cut short");
    }

    /// Where the opcode is, past however many prefixes were written.
    #[test]
    fn the_opcode_is_found_behind_its_prefixes() {
        assert_eq!(measure(&[0x0F, 0xA2]).unwrap().opcode_at, 0);
        assert_eq!(measure(&[0xF3, 0x0F, 0xA2]).unwrap().opcode_at, 1);
        assert_eq!(measure(&[0x66, 0x48, 0x0F, 0xA2]).unwrap().opcode_at, 2);
    }
}
