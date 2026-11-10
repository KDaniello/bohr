//! What a byte stream promises, and the two things that can take it away.
//!
//! Each claim is checked with and without its fault.
//!
//! A stream ends when the node behind it dies: the parked read returns end-of-file.
//!
//! A partition holds the bytes and a heal delivers them; a write into a cut link is neither refused
//! nor lost.
//!
//! A stream carries a vectored answer, as servers send headers and body.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::EventKind;
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{NetworkConfig, NodeFaultKind, ScheduledNodeFault, ScheduledPartition};

/// `read`.
const READ: u32 = 0;
/// `write`.
const WRITE: u32 = 1;
/// `writev`.
const WRITEV: u32 = 20;
/// `close`.
const CLOSE: u32 = 3;
/// `nanosleep`.
const SLEEP: u32 = 35;
/// `getpid`, a marker that the previous call returned.
const MARKER: u32 = 39;
/// `socket`.
const SOCKET: u32 = 41;
/// `connect`.
const CONNECT: u32 = 42;
/// `bind`.
const BIND: u32 = 49;
/// `listen`.
const LISTEN: u32 = 50;
/// `accept4`.
const ACCEPT4: u32 = 288;

/// `AF_INET`, and `SOCK_STREAM` without the flags an event loop would add.
const INET_STREAM: [u64; 6] = [2, 1, 0, 0, 0, 0];

const TIMESPEC_AT: u64 = MEMORY_BASE;
const SOCKADDR_AT: u64 = MEMORY_BASE + 0x100;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;
/// The second half of an answer sent in pieces, and the vector naming both.
const TAIL_AT: u64 = MEMORY_BASE + 0x280;
const IOVEC_AT: u64 = MEMORY_BASE + 0x300;
/// Where a reader puts what it was sent, kept apart from what it sends.
const INBOX_AT: u64 = MEMORY_BASE + 0x400;

/// The descriptors the network model hands out, in the order these scripts ask for them.
///
/// Scripts cannot keep return values. The listener is the server's first socket and the accepted
/// connection its second.
const LISTEN_FD: u64 = 4096;
const ACCEPTED_FD: u64 = 4097;
const CLIENT_FD: u64 = 4096;
/// A second conversation's descriptors; numbers are never reused.
const SECOND_ACCEPTED_FD: u64 = 4098;
const SECOND_CLIENT_FD: u64 = 4097;

/// One scripted sleep, the unit of every wait below.
const STEP: Tick = Tick::MILLISECOND;

/// The server is killed here, well after the connection is established.
const KILL_AT: Tick = Tick(8 * STEP.get());
/// The cut is raised here, after the connection and before the write.
const CUT_AT: Tick = Tick(4 * STEP.get());
/// And lifted here.
const HEAL_AT: Tick = Tick(10 * STEP.get());

fn sleeps(count: usize) -> Vec<GuestOp> {
    (0..count).map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] }).collect()
}

/// Bind, listen, take one connection and read it — then say so.
fn server() -> NullProgram {
    let mut ops = vec![
        GuestOp::Syscall { nr: SOCKET, args: INET_STREAM },
        GuestOp::Syscall { nr: BIND, args: [LISTEN_FD, SOCKADDR_AT, 16, 0, 0, 0] },
        GuestOp::Syscall { nr: LISTEN, args: [LISTEN_FD, 128, 0, 0, 0, 0] },
        // A null address, which `accept4` accepts.
        GuestOp::Syscall { nr: ACCEPT4, args: [LISTEN_FD, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: READ, args: [ACCEPTED_FD, PAYLOAD_AT, 64, 0, 0, 0] },
        GuestOp::Syscall { nr: MARKER, args: [0; 6] },
    ];
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

/// Wait for the server to be listening, connect, and hold the connection open.
fn client(before_write: usize) -> NullProgram {
    let mut ops = sleeps(2);
    ops.push(GuestOp::Syscall { nr: SOCKET, args: INET_STREAM });
    ops.push(GuestOp::Syscall { nr: CONNECT, args: [CLIENT_FD, SOCKADDR_AT, 16, 0, 0, 0] });
    ops.extend(sleeps(before_write));
    ops.push(GuestOp::Syscall { nr: WRITE, args: [CLIENT_FD, PAYLOAD_AT, 5, 0, 0, 0] });
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

/// A client that connects and says nothing, so only the server's death ends the read.
fn silent_client() -> NullProgram {
    let mut ops = sleeps(2);
    ops.push(GuestOp::Syscall { nr: SOCKET, args: INET_STREAM });
    ops.push(GuestOp::Syscall { nr: CONNECT, args: [CLIENT_FD, SOCKADDR_AT, 16, 0, 0, 0] });
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

/// The same server, answering in two pieces once it has heard the request.
fn answering_server() -> NullProgram {
    let mut ops = vec![
        GuestOp::Syscall { nr: SOCKET, args: INET_STREAM },
        GuestOp::Syscall { nr: BIND, args: [LISTEN_FD, SOCKADDR_AT, 16, 0, 0, 0] },
        GuestOp::Syscall { nr: LISTEN, args: [LISTEN_FD, 128, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: ACCEPT4, args: [LISTEN_FD, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: READ, args: [ACCEPTED_FD, INBOX_AT, 64, 0, 0, 0] },
        GuestOp::Syscall { nr: WRITEV, args: [ACCEPTED_FD, IOVEC_AT, 2, 0, 0, 0] },
    ];
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

/// Connect, ask, and wait for the whole answer.
fn asking_client() -> NullProgram {
    let mut ops = sleeps(2);
    ops.push(GuestOp::Syscall { nr: SOCKET, args: INET_STREAM });
    ops.push(GuestOp::Syscall { nr: CONNECT, args: [CLIENT_FD, SOCKADDR_AT, 16, 0, 0, 0] });
    ops.push(GuestOp::Syscall { nr: WRITE, args: [CLIENT_FD, PAYLOAD_AT, 5, 0, 0, 0] });
    // Read after the answer arrives, since a parked read's event lacks the final length.
    ops.extend(sleeps(2));
    ops.push(GuestOp::Syscall { nr: READ, args: [CLIENT_FD, INBOX_AT, 64, 0, 0, 0] });
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

/// Serve one caller, close on it, and serve the next.
fn twice_serving_server() -> NullProgram {
    let mut ops = vec![
        GuestOp::Syscall { nr: SOCKET, args: INET_STREAM },
        GuestOp::Syscall { nr: BIND, args: [LISTEN_FD, SOCKADDR_AT, 16, 0, 0, 0] },
        GuestOp::Syscall { nr: LISTEN, args: [LISTEN_FD, 128, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: ACCEPT4, args: [LISTEN_FD, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: READ, args: [ACCEPTED_FD, INBOX_AT, 64, 0, 0, 0] },
        GuestOp::Syscall { nr: CLOSE, args: [ACCEPTED_FD, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: ACCEPT4, args: [LISTEN_FD, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: READ, args: [SECOND_ACCEPTED_FD, INBOX_AT, 64, 0, 0, 0] },
        GuestOp::Syscall { nr: MARKER, args: [0; 6] },
    ];
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

/// Ask once, hang up, and come back on a connection of its own.
///
/// The second call comes after the server closed the first.
fn twice_asking_client() -> NullProgram {
    let mut ops = sleeps(2);
    ops.push(GuestOp::Syscall { nr: SOCKET, args: INET_STREAM });
    ops.push(GuestOp::Syscall { nr: CONNECT, args: [CLIENT_FD, SOCKADDR_AT, 16, 0, 0, 0] });
    ops.push(GuestOp::Syscall { nr: WRITE, args: [CLIENT_FD, PAYLOAD_AT, 5, 0, 0, 0] });
    ops.push(GuestOp::Syscall { nr: CLOSE, args: [CLIENT_FD, 0, 0, 0, 0, 0] });
    ops.extend(sleeps(4));
    ops.push(GuestOp::Syscall { nr: SOCKET, args: INET_STREAM });
    ops.push(GuestOp::Syscall { nr: CONNECT, args: [SECOND_CLIENT_FD, SOCKADDR_AT, 16, 0, 0, 0] });
    ops.push(GuestOp::Syscall { nr: WRITE, args: [SECOND_CLIENT_FD, PAYLOAD_AT, 5, 0, 0, 0] });
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&STEP.get().to_le_bytes());

    // `sockaddr_in` for node 0: AF_INET, port 7000, 10.66.0.1.
    let mut address = vec![2, 0, 0x1b, 0x58, 10, 66, 0, 1];
    address.extend_from_slice(&[0; 8]);

    // Two `iovec` entries, each a pointer and a length, naming the two halves of one answer.
    let mut vector = Vec::new();
    for (at, len) in [(PAYLOAD_AT, 5u64), (TAIL_AT, 5)] {
        vector.extend_from_slice(&at.to_le_bytes());
        vector.extend_from_slice(&len.to_le_bytes());
    }

    let mut out = Vec::new();
    for node in 0..2u32 {
        out.push((node, TIMESPEC_AT, timespec.clone()));
        out.push((node, SOCKADDR_AT, address.clone()));
        out.push((node, PAYLOAD_AT, b"hello".to_vec()));
        out.push((node, TAIL_AT, b"world".to_vec()));
        out.push((node, IOVEC_AT, vector.clone()));
    }
    out
}

fn judge(programs: Vec<NullProgram>, scenario: Scenario) -> Judged {
    let config = RunConfig {
        horizon: Some(Tick(40 * STEP.get())),
        max_ticks: Tick(Tick::SECOND.get()),
        ..RunConfig::default()
    };
    judge_scenario(programs, &memory(), scenario, &Tape::from_seed(Seed(1)), config)
}

fn two_nodes() -> Scenario {
    Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        ..Scenario::default()
    }
}

/// When the server said it had got past its read, or never.
fn read_returned_at(judged: &Judged) -> Option<Tick> {
    judged
        .events
        .iter()
        .find(|e| e.node == NodeId(0) && matches!(e.kind, EventKind::Syscall { nr: MARKER, .. }))
        .map(|e| e.tick)
}

/// What each of the client's connects answered, in order.
fn client_connects(judged: &Judged) -> Vec<i64> {
    judged
        .events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Syscall { nr: CONNECT, ret, .. } if e.node == NodeId(1) => Some(ret),
            _ => None,
        })
        .collect()
}

/// What the client's read came back with, or nothing where it never returned.
fn client_read(judged: &Judged) -> Option<i64> {
    judged.events.iter().find_map(|e| match e.kind {
        EventKind::Syscall { nr: READ, ret, .. } if e.node == NodeId(1) => Some(ret),
        _ => None,
    })
}

/// A server answers with a vectored write, and the whole answer arrives.
///
/// `writev` on a socket reaches the network. The length checks both halves arrived.
#[test]
fn a_stream_carries_an_answer_written_in_pieces() {
    let judged = judge(vec![answering_server(), asking_client()], two_nodes());
    assert_eq!(client_read(&judged), Some(10), "both pieces of the answer are one stream");
}

/// A listener serves more than one caller.
///
/// Closing an accepted connection keeps the listener's port. Both connects are checked.
#[test]
fn a_listener_serves_more_than_one_caller() {
    let judged = judge(vec![twice_serving_server(), twice_asking_client()], two_nodes());
    assert_eq!(client_connects(&judged), vec![0, 0], "the second caller was refused");
    assert!(read_returned_at(&judged).is_some(), "the server never got past its second read");
}

/// A stream ends when the node behind it dies, and stays open while it lives.
///
/// The same server and silent client, with and without a kill.
#[test]
fn a_stream_ends_when_the_node_behind_it_dies() {
    let killed = Scenario {
        node_faults: vec![ScheduledNodeFault {
            at_tick: KILL_AT,
            node: NodeId(0),
            kind: NodeFaultKind::Kill,
        }],
        ..two_nodes()
    };
    assert!(
        read_returned_at(&judge(vec![server(), silent_client()], killed)).is_none(),
        "the server is the node that died, so it is the client that has to hear about it"
    );

    // From the client's side: it connected and got past its wait.
    let killed = Scenario {
        node_faults: vec![ScheduledNodeFault {
            at_tick: KILL_AT,
            node: NodeId(1),
            kind: NodeFaultKind::Kill,
        }],
        ..two_nodes()
    };
    let ended = judge(vec![server(), silent_client()], killed);
    assert!(
        read_returned_at(&ended).is_some_and(|at| at >= KILL_AT),
        "a server reading a connection whose client died must be released, and only by the death"
    );

    let quiet = judge(vec![server(), silent_client()], two_nodes());
    assert!(
        read_returned_at(&quiet).is_none(),
        "nothing died and nothing was written, so the read had no business returning"
    );
}

/// A partition stops the bytes; a heal carries them on.
///
/// With the cut, the write arrives when it lifts; without it, at once.
#[test]
fn a_partition_holds_a_write_until_the_cut_lifts() {
    let cut = Scenario {
        network: NetworkConfig {
            nodes: 2,
            partitions: vec![ScheduledPartition {
                at_tick: CUT_AT,
                heal_at_tick: HEAL_AT,
                cut: vec![(NodeId(1), NodeId(0)), (NodeId(0), NodeId(1))],
            }],
            ..NetworkConfig::default()
        },
        ..Scenario::default()
    };
    let held = read_returned_at(&judge(vec![server(), client(4)], cut))
        .expect("a write held by a cut still arrives once the cut lifts");
    assert!(
        held >= HEAL_AT,
        "the write was made into a cut link at {CUT_AT:?} and must not arrive before {HEAL_AT:?}, \
         yet the server read it at {held:?}"
    );

    let open = read_returned_at(&judge(vec![server(), client(4)], two_nodes()))
        .expect("with nothing in the way the write arrives");
    assert!(open < HEAL_AT, "with no cut the same write arrives as it is made, not at {open:?}");
}
