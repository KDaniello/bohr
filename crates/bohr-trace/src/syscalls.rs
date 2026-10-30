//! Naming a syscall, and knowing what it is called with, on any host.
//!
//! The crate that owns these names is built for one operating system and one architecture, and it
//! sits below a boundary this crate is above — so it cannot be asked. On every other host it is not
//! even compiled, and a run's syscalls print as bare numbers. A trace and the report made from it
//! are precisely the artefacts that travel, written on the machine that found the bug and read on
//! the maintainer's, so neither can be the thing that loses the names.
//!
//! The copy that follows from that is kept honest mechanically rather than by attention:
//! `scripts/lint-rules.sh` compares the numbers and names against the table they were copied from
//! and fails on any disagreement. It is a data file because a hundred number-name-shape triples are
//! data, and because reformatting would otherwise rewrite the whole table every time one row
//! changed.
//!
//! **The shape is here rather than in the report because the digest needs it.** What a call does
//! not read holds whatever the register held before, and hashing that makes two runs of one program
//! under two memory layouts compare unequal — see [`crate::EventKind::absorb`].

use std::sync::OnceLock;

/// The table, as `number<TAB>name<TAB>shape` lines with `#` comments.
const TABLE: &str = include_str!("../assets/syscalls-x86_64.tsv");

/// What one argument register means to the call that was entered with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg {
    /// A value the call reads as a value: a descriptor, a flag, a length, an offset.
    Scalar,
    /// An address. Where it points is behaviour; what it equals is a fact about this run's memory.
    Address,
}

/// The name of a syscall, where this build has an opinion about it.
///
/// A number with no name is not an error: a scripted guest may issue anything, and a bundle from a
/// newer build may name a call this one has never heard of. The caller shows the number.
#[must_use]
pub fn syscall_name(nr: u32) -> Option<&'static str> {
    entry(nr).map(|(_, name, _)| *name)
}

/// The arguments a call reads, in ABI order, or nothing where this build has never heard of it.
///
/// Nothing means "assume all six", which is stricter than necessary and never wrong: a number this
/// table does not carry is one no scenario here can produce, so the fallback exists for bundles
/// from other builds rather than for runs from this one.
#[must_use]
pub fn syscall_args(nr: u32) -> Option<&'static [Arg]> {
    entry(nr).map(|(_, _, args)| args.as_slice())
}

/// How many calls this build can name, for a check that the table was read at all.
#[must_use]
pub fn named_syscalls() -> usize {
    parsed().len()
}

/// One row, found by binary search.
fn entry(nr: u32) -> Option<&'static (u32, &'static str, Vec<Arg>)> {
    let table = parsed();
    let at = table.binary_search_by_key(&nr, |(number, _, _)| *number).ok()?;
    table.get(at)
}

/// The table, parsed once and sorted so a lookup is a binary search.
fn parsed() -> &'static [(u32, &'static str, Vec<Arg>)] {
    static PARSED: OnceLock<Vec<(u32, &'static str, Vec<Arg>)>> = OnceLock::new();
    PARSED.get_or_init(|| {
        let mut entries: Vec<(u32, &'static str, Vec<Arg>)> = TABLE
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .filter_map(|line| {
                let mut columns = line.split('\t');
                let number = columns.next()?.trim().parse().ok()?;
                let name = columns.next()?.trim();
                let shape = columns.next()?.trim();
                Some((number, name, args_of(shape)))
            })
            .collect();
        entries.sort_unstable_by_key(|(number, _, _)| *number);
        entries.dedup_by_key(|(number, _, _)| *number);
        entries
    })
}

/// One character per argument, and `-` for a call that reads none of the six.
fn args_of(shape: &str) -> Vec<Arg> {
    shape
        .chars()
        .filter_map(|c| match c {
            's' => Some(Arg::Scalar),
            'p' => Some(Arg::Address),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_read_and_looked_up_by_number() {
        assert!(named_syscalls() > 50, "only {} calls were parsed", named_syscalls());
        assert_eq!(syscall_name(0), Some("read"));
        assert_eq!(syscall_name(1), Some("write"));
        assert_eq!(syscall_name(60), Some("exit"));
    }

    /// A number nothing claims is answered with nothing, rather than with a neighbour's name.
    #[test]
    fn an_unknown_number_is_not_guessed_at() {
        assert_eq!(syscall_name(u32::MAX), None);
        assert_eq!(syscall_args(u32::MAX), None);
        assert_eq!(syscall_name(6), None, "a gap in the table is a gap, not the call below it");
    }

    /// The lookup is a binary search, so the order it searches has to be the order it holds.
    #[test]
    fn the_table_is_sorted_and_holds_each_number_once() {
        let table = parsed();
        for pair in table.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} is not before {}", pair[0].1, pair[1].1);
        }
    }

    /// The column the digest reads. Spot-checked against the prototypes rather than exhaustively,
    /// with the three shapes that matter: a call that reads nothing, one that reads a buffer, and
    /// one whose only argument is an address.
    #[test]
    fn a_call_says_which_of_its_registers_it_reads() {
        assert_eq!(syscall_args(24), Some([].as_slice()), "sched_yield takes none of six");
        assert_eq!(
            syscall_args(0),
            Some([Arg::Scalar, Arg::Address, Arg::Scalar].as_slice()),
            "read is a descriptor, a buffer and a count"
        );
        assert_eq!(syscall_args(228), Some([Arg::Scalar, Arg::Address].as_slice()));
        assert_eq!(syscall_args(12), Some([Arg::Address].as_slice()), "brk");
    }

    /// Every row carries a shape, because a row that lost its third column would silently fall back
    /// to all six registers and reintroduce exactly what the column exists to remove.
    #[test]
    fn every_row_names_the_arguments_its_call_reads() {
        let rows = TABLE.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).count();
        assert_eq!(rows, named_syscalls(), "a row was dropped for want of a third column");
    }
}
