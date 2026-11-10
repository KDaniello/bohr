//! Naming a syscall, and knowing what it is called with, on any host.
//!
//! The backend's table compiles only on Linux x86-64, so traces read elsewhere need this copy.
//! `scripts/lint-rules.sh` checks it against the original.
//!
//! The argument shape is here because the digest uses it; see [`crate::EventKind::absorb`].

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
/// `None` for unknown numbers, which the caller prints as is.
#[must_use]
pub fn syscall_name(nr: u32) -> Option<&'static str> {
    entry(nr).map(|(_, name, ..)| *name)
}

/// The arguments a call reads, in ABI order, or nothing where this build has never heard of it.
///
/// `None` means all six, for bundles from other builds.
#[must_use]
pub fn syscall_args(nr: u32) -> Option<&'static [Arg]> {
    entry(nr).map(|(_, _, args, _)| args.as_slice())
}

/// How many calls this build can name, for a check that the table was read at all.
#[must_use]
pub fn named_syscalls() -> usize {
    parsed().len()
}

/// Whether the syscall filter stops the guest at a call and hands it to the world, rather than
/// letting it through to the host; `None` for a number this table does not hold.
#[must_use]
pub fn syscall_traced(nr: u32) -> Option<bool> {
    entry(nr).map(|(_, _, _, traced)| *traced)
}

/// One row, found by binary search.
fn entry(nr: u32) -> Option<&'static Row> {
    let table = parsed();
    let at = table.binary_search_by_key(&nr, |(number, ..)| *number).ok()?;
    table.get(at)
}

/// One row: the number, the name, what the call reads, and whether the filter traces it.
type Row = (u32, &'static str, Vec<Arg>, bool);

/// The table, parsed once and sorted so a lookup is a binary search.
fn parsed() -> &'static [Row] {
    static PARSED: OnceLock<Vec<Row>> = OnceLock::new();
    PARSED.get_or_init(|| {
        let mut entries: Vec<Row> = TABLE
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .filter_map(|line| {
                let mut columns = line.split('\t');
                let number = columns.next()?.trim().parse().ok()?;
                let name = columns.next()?.trim();
                let shape = columns.next()?.trim();
                let traced = columns.next()?.trim() == "trace";
                Some((number, name, args_of(shape), traced))
            })
            .collect();
        entries.sort_unstable_by_key(|(number, ..)| *number);
        entries.dedup_by_key(|(number, ..)| *number);
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

    /// An unknown number gets no name.
    #[test]
    fn an_unknown_number_is_not_guessed_at() {
        assert_eq!(syscall_name(u32::MAX), None);
        assert_eq!(syscall_args(u32::MAX), None);
        assert_eq!(syscall_name(6), None, "a gap in the table is a gap, not the call below it");
    }

    /// The table is sorted.
    #[test]
    fn the_table_is_sorted_and_holds_each_number_once() {
        let table = parsed();
        for pair in table.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} is not before {}", pair[0].1, pair[1].1);
        }
    }

    /// Spot-checks: no arguments, a buffer, an address only.
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

    /// Every row has a shape.
    #[test]
    fn every_row_names_the_arguments_its_call_reads() {
        let rows = TABLE.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).count();
        assert_eq!(rows, named_syscalls(), "a row was dropped for want of a third column");
    }
}
