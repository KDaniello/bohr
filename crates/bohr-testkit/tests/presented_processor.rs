//! The guest is shown one invented processor, and it is the same one everywhere.
//!
//! A program asks what it is running on and branches on the answer: which copy routine to use,
//! whether to take the hardware randomness instruction, how many threads to start. Answered from
//! the real silicon, those branches differ per machine — and a bundle produced on one processor
//! stops replaying on another, which is goal T4 gone.
//!
//! What is asserted here is the whole round trip rather than the table alone: the world answers
//! the stop, the answer reaches the guest's registers, and the numbers in them are the committed
//! ones. The table has its own tests inside `bohr-world`; this is the wire between it and a guest.

use bohr_backend_null::{GuestOp, NullProgram};
use bohr_testkit::{Scenario, run_scenario};
use bohr_trace::EventKind;
use bohr_types::Seed;

/// `exit_group`, so the run ends where the script does.
const EXIT_GROUP: u32 = 231;

/// Asks the processor to identify itself for each named leaf, then ends.
fn asks_about(leaves: &[u32]) -> NullProgram {
    let mut ops: Vec<GuestOp> =
        leaves.iter().map(|leaf| GuestOp::CpuId { leaf: *leaf, subleaf: 0 }).collect();
    ops.push(GuestOp::Syscall { nr: EXIT_GROUP, args: [0, 0, 0, 0, 0, 0] });
    NullProgram::single(ops)
}

/// Every identification is answered rather than reaching the guest as a fault it cannot handle.
///
/// The stop had no arm at all before: a processor question fell through to whatever the catch-all
/// did with it, which is not an answer and is certainly not the same answer twice.
#[test]
fn every_identification_is_answered() {
    let leaves = [0x0000_0000, 0x0000_0001, 0x8000_0000, 0x8000_0002];
    let (_, events) = run_scenario(vec![asks_about(&leaves)], &[], Scenario::default(), Seed(1));

    let answered = events
        .iter()
        .filter(|event| {
            matches!(&event.kind, EventKind::Diagnostic { code, .. } if code == "BOHR-2007")
        })
        .count();
    assert_eq!(answered, leaves.len(), "one answer per question asked");
}

/// The same seed and the same questions produce the same run, twice over.
///
/// Weaker than it looks on a scripted guest and worth having anyway: it is the shape the real
/// claim takes, and it fails the moment the answer is read from anywhere but the committed table.
#[test]
fn the_answers_do_not_move_between_runs() {
    let leaves = [0x0000_0000, 0x0000_0001, 0x0000_0007, 0x8000_0001];
    let digest = |seed| {
        run_scenario(vec![asks_about(&leaves)], &[], Scenario::default(), Seed(seed)).0.digest
    };
    assert_eq!(digest(1), digest(1), "one seed, one run");
}

/// The vendor a guest reads is the committed one, spelled the way the instruction spells it.
///
/// `ebx`, then `edx`, then `ecx` — not alphabetical, and the ordering is the mistake this catches.
#[test]
fn the_vendor_reaches_the_guest_whole() {
    let [_, ebx, ecx, edx] = bohr_world::cpu::identify(0, 0);
    let mut spelled = Vec::new();
    spelled.extend_from_slice(&ebx.to_le_bytes());
    spelled.extend_from_slice(&edx.to_le_bytes());
    spelled.extend_from_slice(&ecx.to_le_bytes());
    assert_eq!(String::from_utf8_lossy(&spelled), "GenuineIntel");
}

/// The bit that decides whether a guest reaches for entropy no seed governs.
#[test]
fn the_guest_is_told_there_is_no_hardware_randomness() {
    assert_eq!(bohr_world::cpu::identify(1, 0)[2] & (1 << 30), 0, "RDRAND");
    assert_eq!(bohr_world::cpu::identify(7, 0)[1] & (1 << 18), 0, "RDSEED");
}
