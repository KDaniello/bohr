//! The digest projection is a tested contract.
//!
//! Behaviour fields change the digest; presentation and host fields do not. The exclusion list is
//! `docs/API.md` §4.4.

use bohr_trace::{
    AssertionKind, DiskOpKind, Event, EventKind, ExitStatus, FaultKind, NodeState, PacketFate,
    SwitchReason, TraceWriter, digest_of,
};
use bohr_types::{EventId, NodeId, SyncWordId, TaskId, Tick, WorldModelId};

fn event(kind: EventKind) -> Event {
    Event { id: EventId(1), tick: Tick(1000), node: NodeId(2), task: TaskId(3), kind }
}

fn digest(kind: EventKind) -> bohr_types::Digest {
    digest_of(&[event(kind)])
}

#[test]
fn diagnostic_wording_is_excluded_and_its_code_is_not() {
    let a = digest(EventKind::Diagnostic {
        code: "BOHR-2005".to_owned(),
        message: "branch counter unavailable".to_owned(),
    });
    let reworded = digest(EventKind::Diagnostic {
        code: "BOHR-2005".to_owned(),
        message: "hardware branch counter is not available on this host".to_owned(),
    });
    let recoded = digest(EventKind::Diagnostic {
        code: "BOHR-2006".to_owned(),
        message: "branch counter unavailable".to_owned(),
    });

    assert_eq!(a, reworded, "rewording a diagnostic must not change what the run did");
    assert_ne!(a, recoded, "a different condition must change the digest");
}

#[test]
fn every_envelope_field_carries_behaviour() {
    let base = event(EventKind::ClockRead { presented: Tick(5) });
    let variants = [
        Event { id: EventId(2), ..base.clone() },
        Event { tick: Tick(1001), ..base.clone() },
        Event { node: NodeId(3), ..base.clone() },
        Event { task: TaskId(4), ..base.clone() },
    ];
    let baseline = digest_of(&[base]);
    for (i, v) in variants.iter().enumerate() {
        assert_ne!(baseline, digest_of(std::slice::from_ref(v)), "envelope field {i} was ignored");
    }
}

#[test]
fn every_syscall_field_carries_behaviour() {
    let base = EventKind::Syscall {
        nr: 228,
        args: [1, 2, 3, 4, 5, 6],
        ret: 0,
        emulated: true,
        data_digest: None,
    };
    let baseline = digest(base.clone());

    // `clock_gettime` reads two arguments; the rest are ignored.
    let mut other_args = [1, 2, 3, 4, 5, 6];
    other_args[0] = 99;

    for (label, kind) in [
        (
            "number",
            EventKind::Syscall {
                nr: 229,
                args: [1, 2, 3, 4, 5, 6],
                ret: 0,
                emulated: true,
                data_digest: None,
            },
        ),
        (
            "args",
            EventKind::Syscall {
                nr: 228,
                args: other_args,
                ret: 0,
                emulated: true,
                data_digest: None,
            },
        ),
        (
            "ret",
            EventKind::Syscall {
                nr: 228,
                args: [1, 2, 3, 4, 5, 6],
                ret: -1,
                emulated: true,
                data_digest: None,
            },
        ),
        (
            "emulated",
            EventKind::Syscall {
                nr: 228,
                args: [1, 2, 3, 4, 5, 6],
                ret: 0,
                emulated: false,
                data_digest: None,
            },
        ),
        (
            "delivered data",
            EventKind::Syscall {
                nr: 228,
                args: [1, 2, 3, 4, 5, 6],
                ret: 0,
                emulated: true,
                data_digest: Some([9; 8]),
            },
        ),
    ] {
        assert_ne!(baseline, digest(kind), "syscall {label} was excluded from the digest");
    }
}

/// Two runs that differ only in where memory landed are the same run.
///
/// Unread argument registers are ignored, so a moved memory layout does not change the digest.
#[test]
fn a_register_a_call_does_not_read_is_not_part_of_which_run_it_was() {
    let called = |args: [u64; 6]| {
        digest(EventKind::Syscall { nr: 24, args, ret: 0, emulated: true, data_digest: None })
    };
    assert_eq!(
        called([0; 6]),
        called([0xdead_beef; 6]),
        "sched_yield reads none of its six registers"
    );

    // Arguments a call reads still change the digest.
    let clock = |args: [u64; 6]| {
        digest(EventKind::Syscall { nr: 228, args, ret: 0, emulated: true, data_digest: None })
    };
    assert_ne!(clock([0, 0x1000, 0, 0, 0, 0]), clock([1, 0x1000, 0, 0, 0, 0]), "the clock read");
    assert_ne!(
        clock([0, 0, 0, 0, 0, 0]),
        clock([0, 0x1000, 0, 0, 0, 0]),
        "somewhere against nowhere"
    );

    // An address value is ignored; `data_digest` carries what it points at.
    assert_eq!(clock([0, 0x1000, 0, 0, 0, 0]), clock([0, 0x7fff_0000, 0, 0, 0, 0]));
}

#[test]
fn variants_do_not_collide_with_each_other() {
    let kinds = vec![
        EventKind::Syscall { nr: 0, args: [0; 6], ret: 0, emulated: false, data_digest: None },
        EventKind::Schedule {
            from: TaskId(0),
            to: TaskId(0),
            reason: SwitchReason::SyscallBoundary,
        },
        EventKind::TaskSpawned { parent: TaskId(0) },
        EventKind::TaskExited { status: ExitStatus::Exited(0) },
        EventKind::ClockRead { presented: Tick(0) },
        EventKind::SdkRecord { index: 0 },
        EventKind::Diagnostic { code: String::new(), message: String::new() },
    ];
    let mut seen = Vec::new();
    for k in kinds {
        let d = digest(k.clone());
        assert!(!seen.contains(&d), "{k:?} collides with an earlier variant");
        seen.push(d);
    }
}

#[test]
fn exit_status_distinguishes_a_code_from_a_signal() {
    let exited = digest(EventKind::TaskExited { status: ExitStatus::Exited(9) });
    let signalled = digest(EventKind::TaskExited { status: ExitStatus::Signalled(9) });
    assert_ne!(exited, signalled, "dying by signal 9 is not exiting with code 9");
}

#[test]
fn domain_payloads_are_digested_without_being_interpreted() {
    let ns = WorldModelId::new("example", 1);
    let a = digest(EventKind::Domain { ns: ns.clone(), kind: 1, payload: vec![1, 2, 3] });
    let b = digest(EventKind::Domain { ns: ns.clone(), kind: 1, payload: vec![1, 2, 4] });
    let c = digest(EventKind::Domain { ns: ns.clone(), kind: 2, payload: vec![1, 2, 3] });
    let d = digest(EventKind::Domain {
        ns: WorldModelId::new("example", 2),
        kind: 1,
        payload: vec![1, 2, 3],
    });

    assert_ne!(a, b, "payload bytes are part of the run");
    assert_ne!(a, c, "the domain event number is part of the run");
    assert_ne!(a, d, "a model version change is a different run");
}

#[test]
fn order_is_part_of_the_run() {
    let a = event(EventKind::ClockRead { presented: Tick(1) });
    let b = Event { id: EventId(2), ..event(EventKind::ClockRead { presented: Tick(2) }) };
    assert_ne!(
        digest_of(&[a.clone(), b.clone()]),
        digest_of(&[b, a]),
        "a permuted event stream is a different run"
    );
}

#[test]
fn the_written_log_digests_the_same_as_the_events_it_holds() {
    let events: Vec<Event> = (0..32)
        .map(|i| Event {
            id: EventId(i),
            tick: Tick(i * 7),
            node: NodeId(0),
            task: TaskId(1),
            kind: EventKind::ClockRead { presented: Tick(i * 7) },
        })
        .collect();

    let mut w = TraceWriter::new(Vec::new());
    for e in &events {
        w.record(e).expect("record");
    }
    let (bytes, streamed, count) = w.finish().expect("finish");

    assert_eq!(count, 32);
    assert_eq!(streamed, digest_of(&events), "streaming and batch digests must agree");
    assert_eq!(
        digest_of(&bohr_trace::read_events(&bytes).expect("read back")),
        streamed,
        "a round trip through the log must not change the digest"
    );
}

/// The nested discriminants, by the number each actually contributes.
///
/// These six reach the digest as `*x as u8`, so they are pinned by number.
#[test]
fn nested_discriminants_are_the_frozen_ones() {
    assert_eq!(SwitchReason::SyscallBoundary as u8, 0);
    assert_eq!(SwitchReason::Blocked as u8, 1);
    assert_eq!(SwitchReason::BudgetExhausted as u8, 2);
    assert_eq!(SwitchReason::Yield as u8, 3);
    assert_eq!(SwitchReason::TaskExit as u8, 4);
    assert_eq!(SwitchReason::FaultPoint as u8, 5);

    assert_eq!(PacketFate::Delivered as u8, 0);
    assert_eq!(PacketFate::Dropped as u8, 1);
    assert_eq!(PacketFate::Duplicated as u8, 2);
    assert_eq!(PacketFate::Reordered as u8, 3);

    assert_eq!(DiskOpKind::Read as u8, 0);
    assert_eq!(DiskOpKind::Write as u8, 1);
    assert_eq!(DiskOpKind::Fsync as u8, 2);
    assert_eq!(DiskOpKind::Truncate as u8, 3);

    assert_eq!(NodeState::Running as u8, 0);
    assert_eq!(NodeState::Frozen as u8, 1);
    assert_eq!(NodeState::Dead as u8, 2);
    assert_eq!(NodeState::Restarting as u8, 3);

    assert_eq!(AssertionKind::Always as u8, 0);
    assert_eq!(AssertionKind::AlwaysOrUnreachable as u8, 1);
    assert_eq!(AssertionKind::Sometimes as u8, 2);
    assert_eq!(AssertionKind::Reachable as u8, 3);
    assert_eq!(AssertionKind::Unreachable as u8, 4);

    assert_eq!(FaultKind::NetDelay as u8, 0);
    assert_eq!(FaultKind::NetDrop as u8, 1);
    assert_eq!(FaultKind::NetDuplicate as u8, 2);
    assert_eq!(FaultKind::NetReorder as u8, 3);
    assert_eq!(FaultKind::NetPartition as u8, 4);
    assert_eq!(FaultKind::DiskTear as u8, 5);
    assert_eq!(FaultKind::DiskReorder as u8, 6);
    assert_eq!(FaultKind::DiskStall as u8, 7);
    assert_eq!(FaultKind::DiskFull as u8, 8);
    assert_eq!(FaultKind::DiskError as u8, 9);
    assert_eq!(FaultKind::NodeKill as u8, 10);
    assert_eq!(FaultKind::NodeFreeze as u8, 11);
    assert_eq!(FaultKind::NodeThaw as u8, 12);
    assert_eq!(FaultKind::NodeRestart as u8, 13);
    assert_eq!(FaultKind::ClockStep as u8, 14);
    assert_eq!(FaultKind::ClockDrift as u8, 15);
    assert_eq!(FaultKind::ChainReorg as u8, 16);
}

/// Which word a task waited on is part of what the run did, and where that word lived is not.
///
/// The address is not hashed, as in `absorb_args`.
#[test]
fn waiting_is_told_apart_by_the_word_and_not_by_where_it_lived() {
    let first = digest(EventKind::Waited { on: SyncWordId(0) });
    let second = digest(EventKind::Waited { on: SyncWordId(1) });
    assert_ne!(first, second, "two different words digested alike");

    let woke_first = digest(EventKind::Woke { task: TaskId(1), on: SyncWordId(0) });
    let woke_other = digest(EventKind::Woke { task: TaskId(2), on: SyncWordId(0) });
    assert_ne!(woke_first, woke_other, "which task was released is part of the run");
    assert_ne!(first, woke_first, "waiting and waking are not the same event");
}

/// Length prefixing, shown on a pair of fields the digest actually absorbs.
///
/// On fields the digest absorbs; `Diagnostic`'s message is excluded.
#[test]
fn adjacent_strings_cannot_be_re_split_to_collide() {
    let ab_c = digest(EventKind::Domain {
        ns: WorldModelId::new("ab", 1),
        kind: 1,
        payload: b"c".to_vec(),
    });
    let a_bc = digest(EventKind::Domain {
        ns: WorldModelId::new("a", 1),
        kind: 1,
        payload: b"bc".to_vec(),
    });
    assert_ne!(ab_c, a_bc, "a namespace and a payload can be re-split to collide");
}

/// Every kind of event, with values in every field, for a digest pinned across releases.
fn every_kind() -> Vec<Event> {
    use bohr_trace::{Endpoint, FaultTarget};
    use bohr_types::Fd;
    let kinds = vec![
        EventKind::Syscall {
            nr: 1,
            args: [3, 0x7fff_0000, 9, 0, 0, 0],
            ret: 9,
            emulated: true,
            data_digest: Some([1, 2, 3, 4, 5, 6, 7, 8]),
        },
        EventKind::Syscall {
            nr: 999,
            args: [1, 2, 3, 4, 5, 6],
            ret: -38,
            emulated: false,
            data_digest: None,
        },
        EventKind::Schedule { from: TaskId(1), to: TaskId(2), reason: SwitchReason::Yield },
        EventKind::TaskSpawned { parent: TaskId(1) },
        EventKind::TaskExited { status: ExitStatus::Exited(2) },
        EventKind::TaskExited { status: ExitStatus::Signalled(11) },
        EventKind::PacketSent {
            from: Endpoint { node: NodeId(0), port: 9000 },
            to: Endpoint { node: NodeId(1), port: 40000 },
            len: 512,
            digest: [9; 8],
        },
        EventKind::PacketFate { fate: PacketFate::Reordered, delay_ticks: 77 },
        EventKind::DiskOp { fd: Fd(5), op: DiskOpKind::Write, offset: 4096, len: 100, ret: 100 },
        EventKind::FaultFired {
            fault: FaultKind::NetPartition,
            target: FaultTarget::Link { from: NodeId(0), to: NodeId(2) },
        },
        EventKind::FaultFired { fault: FaultKind::NodeKill, target: FaultTarget::Node(NodeId(1)) },
        EventKind::FaultFired {
            fault: FaultKind::DiskError,
            target: FaultTarget::Descriptor(Fd(7)),
        },
        EventKind::FaultFired { fault: FaultKind::ClockStep, target: FaultTarget::World },
        EventKind::NodeState { state: NodeState::Frozen },
        EventKind::Waited { on: SyncWordId(4) },
        EventKind::Woke { task: TaskId(3), on: SyncWordId(4) },
        EventKind::ClockRead { presented: Tick(123_456) },
        EventKind::Assertion {
            name: "ordered".to_owned(),
            kind: AssertionKind::Always,
            hit: true,
            condition: false,
        },
        EventKind::SdkRecord { index: 3 },
        EventKind::Diagnostic { code: "BOHR-2012".to_owned(), message: "words".to_owned() },
        EventKind::Domain { ns: WorldModelId::new("example", 2), kind: 7, payload: vec![1, 2, 3] },
        EventKind::Domain { ns: WorldModelId::new("example", 2), kind: 8, payload: vec![0; 300] },
    ];
    kinds
        .into_iter()
        .enumerate()
        .map(|(i, kind)| Event {
            id: EventId(i as u64),
            tick: Tick(1000 + i as u64),
            node: NodeId(i as u32 % 3),
            task: TaskId(i as u32 % 4),
            kind,
        })
        .collect()
}

/// The canonical bytes of every kind of event digest to what they did when this was written: the
/// projection is a format, and a change to how it is fed to the hasher must not move a byte.
#[test]
fn the_digest_of_every_kind_is_pinned() {
    assert_eq!(
        digest_of(&every_kind()).to_string(),
        "blake3:584fa396e5e025be7349d9bd8eb8a1810ea1e043d8a31f019ed47d5aeab44d97"
    );
}
