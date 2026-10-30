//! The JSON Lines projection, which is a promise rather than a rendering: `bohr trace` writes it
//! and a windowed report points at it, so anything that reads one has to keep reading the other.

use bohr_report::jsonl;
use bohr_trace::{
    AssertionKind, DiskOpKind, Endpoint, Event, EventKind, ExitStatus, FaultKind, FaultTarget,
    NodeState, PacketFate, SwitchReason,
};
use bohr_types::{EventId, Fd, NodeId, SyncWordId, TaskId, Tick, WorldModelId};

fn at(id: u64, kind: EventKind) -> Event {
    Event { id: EventId(id), tick: Tick(id * 1000), node: NodeId(1), task: TaskId(2), kind }
}

/// One of every kind, so a variant added without a projection is caught by a test as well as by
/// the compiler.
fn one_of_each() -> Vec<Event> {
    vec![
        at(
            0,
            EventKind::Syscall {
                nr: 1,
                args: [0xffff_ffff_ffff_fff0, 1, 2, 3, 4, 5],
                ret: -11,
                emulated: true,
                data_digest: Some([1, 2, 3, 4, 5, 6, 7, 8]),
            },
        ),
        at(
            1,
            EventKind::Schedule { from: TaskId(1), to: TaskId(2), reason: SwitchReason::Blocked },
        ),
        at(2, EventKind::TaskSpawned { parent: TaskId(1) }),
        at(3, EventKind::TaskExited { status: ExitStatus::Signalled(9) }),
        at(4, EventKind::TaskExited { status: ExitStatus::Exited(3) }),
        at(
            5,
            EventKind::PacketSent {
                from: Endpoint { node: NodeId(0), port: 9000 },
                to: Endpoint { node: NodeId(1), port: 7000 },
                len: 8,
                digest: [9; 8],
            },
        ),
        at(6, EventKind::PacketFate { fate: PacketFate::Dropped, delay_ticks: 0 }),
        at(
            7,
            EventKind::DiskOp { fd: Fd(3), op: DiskOpKind::Fsync, offset: 4096, len: 512, ret: 0 },
        ),
        at(
            8,
            EventKind::FaultFired {
                fault: FaultKind::NodeKill,
                target: FaultTarget::Node(NodeId(1)),
            },
        ),
        at(
            9,
            EventKind::FaultFired {
                fault: FaultKind::NetPartition,
                target: FaultTarget::Link { from: NodeId(0), to: NodeId(1) },
            },
        ),
        at(10, EventKind::NodeState { state: NodeState::Dead }),
        at(11, EventKind::Waited { on: SyncWordId(2) }),
        at(12, EventKind::Woke { task: TaskId(3), on: SyncWordId(2) }),
        at(13, EventKind::ClockRead { presented: Tick(12_000_000_000) }),
        at(
            14,
            EventKind::Assertion {
                name: "log is monotonic".to_owned(),
                kind: AssertionKind::Always,
                hit: true,
                condition: false,
            },
        ),
        at(15, EventKind::SdkRecord { index: 0 }),
        at(
            16,
            EventKind::Diagnostic {
                code: "BOHR-2012".to_owned(),
                message: "always(\"log is monotonic\") failed".to_owned(),
            },
        ),
        at(
            17,
            EventKind::Domain {
                ns: WorldModelId::new("distributed", 1),
                kind: 7,
                payload: vec![0xde, 0xad],
            },
        ),
    ]
}

#[test]
fn every_line_is_json_and_carries_the_envelope() {
    let written = jsonl::write_log(&one_of_each());
    let lines: Vec<&str> = written.lines().collect();
    assert_eq!(lines.len(), one_of_each().len(), "one line per event");

    for line in lines {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        let object = value.as_object().expect("an event is an object");
        for field in ["id", "tick", "node", "task", "kind"] {
            assert!(object.contains_key(field), "{line} has no {field}");
        }
        let kind = object["kind"].as_str().expect("kind is a name");
        assert!(object.contains_key(kind), "{line} carries no body under its own kind");
        assert!(
            kind.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{kind} is not the snake case the format uses"
        );
    }
}

/// Numbers wide enough to lose precision as doubles are strings, and the ones a reader sorts by
/// are not. A syscall argument silently rounded by being read is a value the reader cannot know
/// was changed.
#[test]
fn what_would_not_survive_being_read_as_a_double_is_a_string() {
    let written = jsonl::write_log(&one_of_each());
    let syscall: serde_json::Value =
        serde_json::from_str(written.lines().next().expect("the first event")).expect("json");

    let args = syscall["syscall"]["args"].as_array().expect("args is an array");
    assert_eq!(args[0].as_str(), Some("0xfffffffffffffff0"), "a wide argument was not a string");
    assert!(syscall["id"].is_number(), "an identifier a reader sorts by must stay a number");
    assert!(syscall["tick"].is_number(), "virtual time is bounded and stays a number");
    assert_eq!(syscall["syscall"]["name"].as_str(), Some("write"), "the call was not named");
}

/// Everything in a string field came out of a guest, which has no interest in producing valid
/// JSON. This is the field a hostile one aims at.
#[test]
fn a_guest_cannot_break_the_line_it_is_written_on() {
    let hostile = "\"}\n{\"id\":999,\"kind\":\"forged\",\"x\":\"\u{202e}\u{0}";
    let written = jsonl::write_log(&[at(
        1,
        EventKind::Assertion {
            name: hostile.to_owned(),
            kind: AssertionKind::Always,
            hit: true,
            condition: false,
        },
    )]);

    assert_eq!(written.lines().count(), 1, "the guest's own text opened a second line");
    let value: serde_json::Value =
        serde_json::from_str(written.trim_end()).expect("the line is still json");
    assert_eq!(value["assertion"]["name"].as_str(), Some(hostile), "the text did not survive");
}

/// The same events written twice are the same bytes: a report and its sidecar are compared, and a
/// projection that reordered anything would make that comparison meaningless.
#[test]
fn the_projection_is_the_same_bytes_every_time() {
    assert_eq!(jsonl::write_log(&one_of_each()), jsonl::write_log(&one_of_each()));
}
