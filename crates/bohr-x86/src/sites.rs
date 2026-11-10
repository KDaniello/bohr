//! Where an instruction begins, and whether that was established.
//!
//! A search for an opcode's bytes finds every real instruction and some displacements too. Decoding
//! the code from a known instruction boundary and seeing where the steps land tells them apart.
//!
//! A diagnosis reports every candidate, confirmed or not. A rewrite touches only confirmed ones,
//! because overwriting the middle of another instruction corrupts it silently. [`Site::confirmed`]
//! carries the difference.

use super::{insn, symbols};

/// Where an instruction sits, how long it is, and whether that was established.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Site {
    /// Offset within the object.
    pub at: usize,
    /// Bytes, prefixes included.
    pub len: usize,
    /// Whether a sweep measured an instruction beginning here. Rewrites touch only confirmed
    /// sites.
    pub confirmed: bool,
}

/// The candidates that survive measurement, each marked confirmed or not.
///
/// A site is confirmed from a function symbol covering it, by sweeping from the symbol. Without a
/// symbol (a stripped library, or `libstdc++`'s unexported `std::random_device` code), sweeps
/// started at several bytes before it must agree.
///
/// A candidate inside a swept function that is not a boundary is dropped. A candidate no sweep
/// reached is kept, unconfirmed.
#[must_use]
pub fn confirm(object: &[u8], candidates: Vec<Site>) -> Vec<Site> {
    if candidates.is_empty() {
        return Vec::new();
    }
    // An object without symbols still goes through the agreement check below.
    let functions = symbols::functions(object);

    let mut starts: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    let mut judged: Vec<(usize, usize)> = Vec::new();
    for function in &functions {
        let code = &object[function.at..function.at + function.len];
        let mut at = 0;
        let mut reached = function.at;
        while at < code.len() {
            let Some(step) = insn::measure(&code[at..]) else {
                break;
            };
            starts.insert(function.at + at, step.len);
            reached = function.at + at;
            at += step.len;
        }
        judged.push((function.at, reached));
    }

    candidates
        .into_iter()
        .filter_map(|site| {
            // The opcode may follow a `REX` prefix. The measurement decides, since many bytes look
            // like a prefix without being one.
            if let Some(len) = starts.get(&site.at) {
                return Some(Site { at: site.at, len: *len, confirmed: true });
            }
            if site.at > 0
                && let Some(len) = starts.get(&(site.at - 1))
                && *len == site.len + 1
            {
                return Some(Site { at: site.at - 1, len: *len, confirmed: true });
            }
            let inside = judged.iter().any(|(from, to)| site.at >= *from && site.at < *to);
            if inside {
                return None;
            }
            Some(Site { confirmed: agreed(object, site.at), ..site })
        })
        .collect()
}

/// Whether sweeps begun at several different bytes all agree that an instruction begins here.
///
/// A sweep started mid-instruction decodes garbage for a step or two, then falls into step with the
/// real boundaries. Every start that reaches the address must land on it, and at least `ENOUGH`
/// of them must.
fn agreed(object: &[u8], at: usize) -> bool {
    /// How far back each sweep starts. Spread out, since starts one byte apart often resynchronise
    /// on the same wrong boundaries.
    const BACK: [usize; 5] = [16, 24, 32, 48, 64];
    /// The minimum number of starts that must land on the address.
    const ENOUGH: usize = 3;

    let mut reached = 0;
    for back in BACK {
        let Some(from) = at.checked_sub(back) else {
            continue;
        };
        let mut walk = from;
        while walk < at {
            let Some(step) = insn::measure(&object[walk..]) else {
                break;
            };
            walk += step.len;
        }
        // This sweep stepped over the address, so no instruction begins there.
        if walk > at {
            return false;
        }
        if walk == at {
            reached += 1;
        }
    }
    reached >= ENOUGH
}

/// The ranges of a file that become instructions once it is mapped.
///
/// Bytes in string tables or relocations are skipped. A file that is not a readable ELF object is
/// scanned whole, since its code could be anywhere.
#[must_use]
pub fn executable(object: &[u8]) -> Vec<(usize, usize)> {
    let header = |at: usize, len: usize| -> u64 {
        object.get(at..at + len).map_or(0, |b| {
            let mut wide = [0u8; 8];
            wide[..len].copy_from_slice(b);
            u64::from_le_bytes(wide)
        })
    };
    if object.len() < 64 || &object[..4] != b"\x7fELF" || object[4] != 2 {
        return vec![(0, object.len())];
    }
    let off = header(0x20, 8) as usize;
    let size = header(0x36, 2) as usize;
    let count = header(0x38, 2) as usize;
    let mut out = Vec::new();
    for i in 0..count {
        let at = off + i * size;
        // `PT_LOAD` carrying the executable bit.
        if header(at, 4) != 1 || header(at + 4, 4) & 1 == 0 {
            continue;
        }
        let from = header(at + 0x08, 8) as usize;
        let to = (from + header(at + 0x20, 8) as usize).min(object.len());
        if from < to {
            out.push((from, to));
        }
    }
    if out.is_empty() {
        out.push((0, object.len()));
    }
    out
}
