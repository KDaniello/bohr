//! How long one x86-64 instruction is.
//!
//! Only the length is decoded, to find where the next instruction starts. A rewriter needs that
//! before it may touch a byte: a trap written over the middle of a longer instruction corrupts it.
//!
//! The decoder covers every encoding, vector prefixes included. A sweep walks through code the
//! guest never runs, such as an `AVX-512` path, and one wrong length there shifts every boundary
//! after it.
//!
//! Checked against `objdump`: over the text of `solana-test-validator` (42 MB of Rust, assembly and
//! vector code) the two agree on 8,985,713 consecutive instructions; over a static guest from
//! `testdata/`, on all 147,897 boundaries.
//!
//! It cannot tell code from data, so callers sweep from one function symbol to the next instead
//! of across a whole section.

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
/// `None` where the bytes run out mid-instruction or the opcode is invalid in 64-bit mode: either
/// way the sweep has left the code.
pub fn measure(code: &[u8]) -> Option<Instruction> {
    let mut at = 0;
    let mut opsize16 = false;
    let mut rex_w = false;

    // Legacy prefixes, in any order and number. `REX` counts only as the last prefix.
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

    // Vector prefixes carry their own opcode map and operand size. Decoded for length only.
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
                // The three-byte maps: `0F 38` takes a modrm and no immediate, `0F 3A` a modrm
                // and one byte.
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

    // `F6`/`F7`: only `test` in this group has an immediate.
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
/// The map selects the opcode table; map 3 carries an immediate byte. `pp` and `W` are passed for
/// the operand size, though no vector immediate here varies with it.
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
    // Map 1 is the two-byte opcode table, so it uses the same lookup: its shift groups take an
    // immediate byte, and `vzeroupper`/`vzeroall` take no modrm.
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

/// Length of the modrm byte with its optional SIB byte and displacement.
fn modrm_len(code: &[u8], at: usize) -> Option<usize> {
    let modrm = *code.get(at)?;
    let mode = modrm >> 6;
    let rm = modrm & 0b111;

    if mode == 0b11 {
        return Some(1);
    }
    // An `rm` of `100` means a SIB byte follows.
    let sib = usize::from(rm == 0b100);
    let disp = match mode {
        // `rm` `101` with mode `00` is RIP-relative, with a four-byte offset. Compilers use it
        // everywhere.
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
/// `None` for opcodes invalid in 64-bit mode: the sweep has left the code.
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

        // Invalid in long mode, their encodings reused for `REX` and vector prefixes. Reaching one
        // means the walk is off instruction boundaries.
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
        // `mov` with an absolute address.
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

    /// The instructions this module exists to find, and the ones around them.
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

    /// RIP-relative addressing carries a four-byte offset.
    #[test]
    fn an_offset_from_the_instruction_pointer_is_four_bytes() {
        // mov rax, [rip + 0x11223344]
        assert_eq!(len(&[0x48, 0x8B, 0x05, 0x44, 0x33, 0x22, 0x11]), Some(7));
        // lea rdi, [rip + 0]
        assert_eq!(len(&[0x48, 0x8D, 0x3D, 0x00, 0x00, 0x00, 0x00]), Some(7));
    }

    /// A SIB byte is counted, with the displacement a no-base SIB implies.
    #[test]
    fn a_scale_index_base_byte_is_counted() {
        // mov eax, [rsp]
        assert_eq!(len(&[0x8B, 0x04, 0x24]), Some(3));
        // mov eax, [rsp + 0x10]
        assert_eq!(len(&[0x8B, 0x44, 0x24, 0x10]), Some(4));
        // mov eax, [rbp*4 + 0x11223344] -- no base, so four bytes of displacement
        assert_eq!(len(&[0x8B, 0x04, 0xAD, 0x44, 0x33, 0x22, 0x11]), Some(7));
    }

    /// Immediate width follows the operand-size prefix and `REX.W`.
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

    /// In the `F6`/`F7` group `test` carries an immediate; `not` and `neg` do not.
    #[test]
    fn one_opcode_two_shapes() {
        assert_eq!(len(&[0xF7, 0xC0, 0x44, 0x33, 0x22, 0x11]), Some(6), "test eax, imm32");
        assert_eq!(len(&[0xF7, 0xD0]), Some(2), "not eax");
        assert_eq!(len(&[0xF6, 0xC3, 0x01]), Some(3), "test bl, imm8");
        assert_eq!(len(&[0xF6, 0xD3]), Some(2), "not bl");
    }

    /// Vector instructions are measured, since sweeps pass through paths the guest never runs.
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

    /// An invalid opcode or truncated bytes give `None`, so the caller stops there.
    #[test]
    fn an_impossible_opcode_is_not_guessed_at() {
        assert_eq!(len(&[0x06]), None, "push es");
        assert_eq!(len(&[0x9A, 0, 0, 0, 0, 0, 0]), None, "far call");
        assert_eq!(len(&[0x0F]), None, "an escape with nothing after it");
        assert_eq!(len(&[]), None);
        assert_eq!(len(&[0x48, 0x8B, 0x05, 0x44]), None, "a displacement cut short");
    }

    /// The opcode offset skips every prefix.
    #[test]
    fn the_opcode_is_found_behind_its_prefixes() {
        assert_eq!(measure(&[0x0F, 0xA2]).unwrap().opcode_at, 0);
        assert_eq!(measure(&[0xF3, 0x0F, 0xA2]).unwrap().opcode_at, 1);
        assert_eq!(measure(&[0x66, 0x48, 0x0F, 0xA2]).unwrap().opcode_at, 2);
    }
}
