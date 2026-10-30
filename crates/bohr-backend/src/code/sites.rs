//! Where an instruction begins, and how sure of that anyone is.
//!
//! One question, asked the same way whatever instruction is being looked for. A search for an
//! opcode is a couple of bytes wide and finds every real one and some displacements besides; what
//! separates them is measuring the code from a place known to be a boundary and seeing where the
//! steps land.
//!
//! **Reporting and rewriting are not the same question.** A diagnosis says everything it found,
//! the unestablished included, because a site real and unreported is a hole described as solid
//! ground. A rewrite touches only what was established, because bytes written over the middle of
//! another instruction corrupt it silently and nowhere near where the mistake was made.
//! [`Site::confirmed`] is that difference, and both callers exist.

use super::{insn, symbols};

/// Where an instruction sits, how long it is, and whether that was established.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Site {
    /// Offset within the object.
    pub at: usize,
    /// Bytes, prefixes included.
    pub len: usize,
    /// Whether a sweep measured an instruction beginning here.
    ///
    /// A diagnosis reports the unconfirmed as well. A rewrite must not touch them: see the note at
    /// the head of this module for why the two callers are given different answers.
    pub confirmed: bool,
}

/// Every candidate that survives being measured, and whether each was established.
///
/// Two ways of establishing one, in order of strength. From a function symbol, where the object
/// keeps one covering the site: the sweep runs from the symbol and the site is a boundary or it is
/// not. Where no symbol covers it — a stripped shared object exports little, and `libstdc++` keeps
/// the five sites `std::random_device` reaches somewhere unexported — by agreement between sweeps
/// begun at several different bytes before it.
///
/// A candidate inside a swept function that is not one of its boundaries is dropped: it has been
/// shown to be the middle of something longer. A candidate nothing reached is kept, unconfirmed.
#[must_use]
pub fn confirm(object: &[u8], candidates: Vec<Site>) -> Vec<Site> {
    if candidates.is_empty() {
        return Vec::new();
    }
    // No early answer where there are no symbols. An object with none still has code, and the
    // agreement below is what reads it -- returning the raw candidates there would hand a rewrite
    // exactly the bytes it must not touch.
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
            // The opcode may sit one byte into the instruction, behind a `REX` prefix. Which of the
            // two it is is read off the measurement rather than guessed from the byte before:
            // plenty of bytes look like that prefix without being one, and a site placed a byte
            // early is a site the check then answers about the wrong address.
            if let Some(len) = starts.get(&site.at) {
                return Some(Site { at: site.at, len: *len, confirmed: true });
            }
            if site.at > 0 {
                if let Some(len) = starts.get(&(site.at - 1)) {
                    if *len == site.len + 1 {
                        return Some(Site { at: site.at - 1, len: *len, confirmed: true });
                    }
                }
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
/// Weaker than a symbol and much stronger than a guess. A sweep started in the middle of an
/// instruction produces nonsense for a step or two and then falls back into step with the real
/// boundaries, so starts that disagree are starts still inside their own nonsense. What is required
/// is that every start reaching this far lands on it, and that enough of them do to be worth
/// calling agreement.
fn agreed(object: &[u8], at: usize) -> bool {
    /// How far back each attempt begins. Spread rather than consecutive: two starts one byte apart
    /// resynchronise into the same nonsense as often as into the same truth.
    const BACK: [usize; 5] = [16, 24, 32, 48, 64];
    /// Below this many usable starts there is no agreement to speak of, only a small sample.
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
        // Past it means this start stepped over the address: an instruction it measured covered the
        // byte, so from where it stood nothing begins here.
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
/// Only those. An opcode spelled inside a string table or a relocation is not an instruction and
/// never will be, and counting it makes a diagnosis louder without making it truer. A file that is
/// not an object this can read is scanned whole, because nothing said where its code is and silence
/// is not the same as absence.
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
