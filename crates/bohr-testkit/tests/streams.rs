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
//!
//! A reader that falls behind loses no byte: the writer waits for room, or is told there is none.
//! Each direction of a stream closes on its own, and a receive's flags mean what Linux makes them
//! mean.

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
/// `poll`.
const POLL: u32 = 7;
/// `sendto`.
const SENDTO: u32 = 44;
/// `recvfrom`.
const RECVFROM: u32 = 45;
/// `shutdown`.
const SHUTDOWN: u32 = 48;

/// `AF_INET`, and `SOCK_STREAM` without the flags an event loop would add.
const INET_STREAM: [u64; 6] = [2, 1, 0, 0, 0, 0];
/// The same with `SOCK_NONBLOCK`.
const INET_STREAM_NONBLOCK: [u64; 6] = [2, 1 | 0o4000, 0, 0, 0, 0];
/// `AF_INET`, `SOCK_DGRAM`.
const INET_DATAGRAM: [u64; 6] = [2, 2, 0, 0, 0, 0];

const MSG_PEEK: u64 = 0x2;
const MSG_TRUNC: u64 = 0x20;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_WAITALL: u64 = 0x100;
const SHUT_RD: u64 = 0;
const SHUT_WR: u64 = 1;
const EAGAIN: i64 = -11;
/// `POLLOUT`.
const POLLOUT: u16 = 0x4;

const TIMESPEC_AT: u64 = MEMORY_BASE;
const SOCKADDR_AT: u64 = MEMORY_BASE + 0x100;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;
/// The second half of an answer sent in pieces, and the vector naming both.
const TAIL_AT: u64 = MEMORY_BASE + 0x280;
const IOVEC_AT: u64 = MEMORY_BASE + 0x300;
/// Where a reader puts what it was sent, kept apart from what it sends.
const INBOX_AT: u64 = MEMORY_BASE + 0x400;
/// One `pollfd` naming the client's stream and asking whether it takes a write.
const POLLFD_AT: u64 = MEMORY_BASE + 0x500;
/// A chunk a bulk writer sends again and again, and the buffer a bulk reader fills.
const CHUNK_AT: u64 = MEMORY_BASE + 0x1000;
const CHUNK: u64 = 16 * 1024;
const BULK_IN_AT: u64 = MEMORY_BASE + 0x8000;
const BULK_READ: u64 = 32 * 1024;
/// A `struct timeval` of 3 ms, for `SO_RCVTIMEO`; an option value and its length, for `getsockopt`.
const TIMEVAL_AT: u64 = MEMORY_BASE + 0x700;
const OPT_AT: u64 = MEMORY_BASE + 0x740;
const OPT_LEN_AT: u64 = MEMORY_BASE + 0x780;
/// What one direction of a stream holds unread: its receive buffer and the writer's send buffer,
/// 1 MiB each.
const IN_FLIGHT: u64 = 2 << 20;

/// The descriptors these scripts are handed, the lowest free number each time, as a process
/// numbers them.
///
/// Scripts cannot keep return values. The listener is the server's first descriptor and the
/// accepted connection its second.
const LISTEN_FD: u64 = 3;
const ACCEPTED_FD: u64 = 4;
const CLIENT_FD: u64 = 3;
/// A second conversation's descriptors, which take the numbers the first one closed.
const SECOND_ACCEPTED_FD: u64 = 4;
const SECOND_CLIENT_FD: u64 = 3;

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

    // `pollfd`: the client's first descriptor, `POLLOUT`, nothing returned yet.
    let mut pollfd = (CLIENT_FD as i32).to_le_bytes().to_vec();
    pollfd.extend_from_slice(&POLLOUT.to_le_bytes());
    pollfd.extend_from_slice(&0u16.to_le_bytes());

    let chunk: Vec<u8> = (0..CHUNK).map(|i| (i % 251) as u8).collect();

    let mut out = Vec::new();
    for node in 0..2u32 {
        out.push((node, TIMESPEC_AT, timespec.clone()));
        out.push((node, SOCKADDR_AT, address.clone()));
        out.push((node, PAYLOAD_AT, b"hello".to_vec()));
        out.push((node, TAIL_AT, b"world".to_vec()));
        out.push((node, IOVEC_AT, vector.clone()));
        out.push((node, POLLFD_AT, pollfd.clone()));
        out.push((node, CHUNK_AT, chunk.clone()));
        let mut timeval = 0i64.to_le_bytes().to_vec();
        timeval.extend_from_slice(&3_000i64.to_le_bytes());
        out.push((node, TIMEVAL_AT, timeval));
        out.push((node, OPT_LEN_AT, 4u32.to_le_bytes().to_vec()));
    }
    out
}

/// Bind, listen and take one connection, the opening of every server below.
fn accepting() -> Vec<GuestOp> {
    vec![
        GuestOp::Syscall { nr: SOCKET, args: INET_STREAM },
        GuestOp::Syscall { nr: BIND, args: [LISTEN_FD, SOCKADDR_AT, 16, 0, 0, 0] },
        GuestOp::Syscall { nr: LISTEN, args: [LISTEN_FD, 128, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: ACCEPT4, args: [LISTEN_FD, 0, 0, 0, 0, 0] },
    ]
}

/// Wait for the server, open a stream of the given kind and connect it.
fn connecting(kind: [u64; 6]) -> Vec<GuestOp> {
    let mut ops = sleeps(2);
    ops.push(GuestOp::Syscall { nr: SOCKET, args: kind });
    ops.push(GuestOp::Syscall { nr: CONNECT, args: [CLIENT_FD, SOCKADDR_AT, 16, 0, 0, 0] });
    ops
}

/// `count` chunk writes on the client's stream.
fn chunk_writes(count: usize) -> Vec<GuestOp> {
    (0..count)
        .map(|_| GuestOp::Syscall { nr: WRITE, args: [CLIENT_FD, CHUNK_AT, CHUNK, 0, 0, 0] })
        .collect()
}

/// `count` bulk reads on the accepted stream.
fn bulk_reads(count: usize) -> Vec<GuestOp> {
    (0..count)
        .map(|_| GuestOp::Syscall { nr: READ, args: [ACCEPTED_FD, BULK_IN_AT, BULK_READ, 0, 0, 0] })
        .collect()
}

/// A server that sleeps through the first `late` steps, then reads `MiB` mebibytes a mebibyte at a
/// time, resting between them, and says so.
fn slow_reader(late: usize, mib: usize) -> NullProgram {
    let mut ops = accepting();
    ops.extend(sleeps(late));
    let per_mib = ((1 << 20) / BULK_READ) as usize;
    for _ in 0..mib {
        ops.extend(bulk_reads(per_mib));
        ops.extend(sleeps(5));
    }
    ops.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

/// A server that takes the connection and never reads it.
fn idle_server() -> NullProgram {
    let mut ops = accepting();
    ops.extend(sleeps(30));
    NullProgram::single(ops)
}

/// What every call numbered `nr` on `node` answered, in order; a parked call answers 0 here.
fn answers(judged: &Judged, node: u32, nr: u32) -> Vec<i64> {
    judged
        .events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Syscall { nr: n, ret, .. } if n == nr && e.node == NodeId(node) => Some(ret),
            _ => None,
        })
        .collect()
}

/// When `node` said it had got past its last call, or never.
fn marked_at(judged: &Judged, node: u32) -> Option<Tick> {
    judged
        .events
        .iter()
        .find(|e| e.node == NodeId(node) && matches!(e.kind, EventKind::Syscall { nr: MARKER, .. }))
        .map(|e| e.tick)
}

fn judge(programs: Vec<NullProgram>, scenario: Scenario) -> Judged {
    let config = RunConfig {
        horizon: Some(Tick(40 * STEP.get())),
        max_ticks: Tick(Tick::SECOND.get()),
        ..RunConfig::default()
    };
    // Memory goes only to the nodes the scenario runs.
    let nodes = programs.len() as u32;
    let memory: Vec<_> = memory().into_iter().filter(|(node, ..)| *node < nodes).collect();
    judge_scenario(programs, &memory, scenario, &Tape::from_seed(Seed(1)), config)
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

/// A reader that falls behind loses no byte: the writer waits for room.
///
/// The client writes 3 MiB, more than the stream holds, while the server sleeps; every byte is
/// read.
#[test]
fn a_reader_that_falls_behind_loses_no_byte() {
    let mut writer = connecting(INET_STREAM);
    writer.extend(chunk_writes(192));
    writer.extend(sleeps(30));
    let judged = judge(vec![slow_reader(10, 3), NullProgram::single(writer)], two_nodes());

    let read: i64 = answers(&judged, 0, READ).iter().sum();
    assert_eq!(read, 3 << 20, "the reader got {read} of the 3 MiB written");
    assert!(marked_at(&judged, 0).is_some(), "a read found the stream empty with bytes owed");
}

/// A writer that may not wait is told the stream is full, and so is `poll`.
///
/// Both buffers fill at 2 MiB; every write past them answers `EAGAIN`.
#[test]
fn a_writer_that_may_not_wait_is_told_the_stream_is_full() {
    let mut writer = connecting(INET_STREAM_NONBLOCK);
    writer.extend(chunk_writes(140));
    writer.push(GuestOp::Syscall { nr: POLL, args: [POLLFD_AT, 1, 0, 0, 0, 0] });
    writer.extend(sleeps(30));
    let judged = judge(vec![idle_server(), NullProgram::single(writer)], two_nodes());

    let written = answers(&judged, 1, WRITE);
    let fits = (IN_FLIGHT / CHUNK) as usize;
    assert!(written[..fits].iter().all(|r| *r == CHUNK as i64), "a write within room fell short");
    assert!(written[fits..].iter().all(|r| *r == EAGAIN), "a write past room answered {written:?}");
    assert_eq!(answers(&judged, 1, POLL), vec![0], "a full stream was reported writable");
}

/// A blocking writer asking not to wait is told the stream is full.
#[test]
fn a_send_asked_not_to_wait_is_told_the_stream_is_full() {
    let mut writer = connecting(INET_STREAM);
    writer.extend(chunk_writes((IN_FLIGHT / CHUNK) as usize));
    writer.push(GuestOp::Syscall {
        nr: SENDTO,
        args: [CLIENT_FD, CHUNK_AT, CHUNK, MSG_DONTWAIT, 0, 0],
    });
    writer.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    writer.extend(sleeps(30));
    let judged = judge(vec![idle_server(), NullProgram::single(writer)], two_nodes());

    assert_eq!(answers(&judged, 1, SENDTO), vec![EAGAIN]);
    assert!(marked_at(&judged, 1).is_some(), "a send asked not to wait waited");
}

/// A cut holds a blocking writer until the heal instead of failing it, and loses nothing.
#[test]
fn a_cut_holds_a_blocking_writer_instead_of_failing_it() {
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
    let mut writer = connecting(INET_STREAM);
    writer.extend(sleeps(3));
    writer.extend(chunk_writes(192));
    writer.extend(sleeps(30));
    let judged = judge(vec![slow_reader(14, 3), NullProgram::single(writer)], cut);

    assert!(!answers(&judged, 1, WRITE).contains(&EAGAIN), "a blocking writer was told to retry");
    let read: i64 = answers(&judged, 0, READ).iter().sum();
    assert_eq!(read, 3 << 20, "the reader got {read} of the 3 MiB written across the cut");
}

/// A stream half closed by its client still carries the server's reply.
///
/// `shutdown(SHUT_WR)` ends the request, the server reads it to its end and answers.
#[test]
fn a_half_closed_stream_still_carries_the_reply() {
    let mut server = accepting();
    server.extend(sleeps(4));
    server.push(GuestOp::Syscall { nr: READ, args: [ACCEPTED_FD, INBOX_AT, 64, 0, 0, 0] });
    server.push(GuestOp::Syscall { nr: READ, args: [ACCEPTED_FD, INBOX_AT, 64, 0, 0, 0] });
    server.push(GuestOp::Syscall { nr: WRITE, args: [ACCEPTED_FD, PAYLOAD_AT, 5, 0, 0, 0] });
    server.push(GuestOp::Syscall { nr: CLOSE, args: [ACCEPTED_FD, 0, 0, 0, 0, 0] });
    server.extend(sleeps(30));

    let mut client = connecting(INET_STREAM);
    client.push(GuestOp::Syscall { nr: WRITE, args: [CLIENT_FD, PAYLOAD_AT, 5, 0, 0, 0] });
    client.push(GuestOp::Syscall { nr: SHUTDOWN, args: [CLIENT_FD, SHUT_WR, 0, 0, 0, 0] });
    client.extend(sleeps(6));
    client.push(GuestOp::Syscall { nr: READ, args: [CLIENT_FD, INBOX_AT, 64, 0, 0, 0] });
    client.push(GuestOp::Syscall { nr: READ, args: [CLIENT_FD, INBOX_AT, 64, 0, 0, 0] });
    client.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    client.extend(sleeps(30));

    let judged = judge(vec![NullProgram::single(server), NullProgram::single(client)], two_nodes());
    assert_eq!(answers(&judged, 0, READ), vec![5, 0], "the server reads the request, then its end");
    assert_eq!(answers(&judged, 0, WRITE), vec![5], "the reply went into a stream still open");
    assert_eq!(answers(&judged, 1, READ), vec![5, 0], "the client reads the reply, then its end");
    assert!(marked_at(&judged, 1).is_some());
}

/// Shutting down the reading half ends this side's reads and tells the peer nothing.
#[test]
fn shutting_down_the_reading_half_tells_the_peer_nothing() {
    let mut server = accepting();
    server.push(GuestOp::Syscall { nr: READ, args: [ACCEPTED_FD, INBOX_AT, 64, 0, 0, 0] });
    server.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    server.extend(sleeps(30));

    let mut client = connecting(INET_STREAM);
    client.push(GuestOp::Syscall { nr: SHUTDOWN, args: [CLIENT_FD, SHUT_RD, 0, 0, 0, 0] });
    client.push(GuestOp::Syscall { nr: READ, args: [CLIENT_FD, INBOX_AT, 64, 0, 0, 0] });
    client.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    client.extend(sleeps(30));

    let judged = judge(vec![NullProgram::single(server), NullProgram::single(client)], two_nodes());
    assert!(marked_at(&judged, 1).is_some(), "a read after SHUT_RD waited instead of ending");
    assert!(marked_at(&judged, 0).is_none(), "the peer was told the stream ended");
}

/// A peek leaves the bytes for the next read.
#[test]
fn a_peek_leaves_the_bytes_for_the_next_read() {
    let mut server = accepting();
    server.extend(sleeps(4));
    server
        .push(GuestOp::Syscall { nr: RECVFROM, args: [ACCEPTED_FD, INBOX_AT, 64, MSG_PEEK, 0, 0] });
    server.push(GuestOp::Syscall { nr: RECVFROM, args: [ACCEPTED_FD, INBOX_AT, 64, 0, 0, 0] });
    server.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    server.extend(sleeps(30));

    let mut client = connecting(INET_STREAM);
    client.push(GuestOp::Syscall { nr: WRITE, args: [CLIENT_FD, PAYLOAD_AT, 5, 0, 0, 0] });
    client.extend(sleeps(30));

    let judged = judge(vec![NullProgram::single(server), NullProgram::single(client)], two_nodes());
    assert_eq!(answers(&judged, 0, RECVFROM), vec![5, 5]);
    assert!(marked_at(&judged, 0).is_some());
}

/// A receive asked not to wait answers `EAGAIN` on an empty blocking stream.
#[test]
fn a_receive_asked_not_to_wait_does_not_wait() {
    let mut client = connecting(INET_STREAM);
    client.push(GuestOp::Syscall {
        nr: RECVFROM,
        args: [CLIENT_FD, INBOX_AT, 64, MSG_DONTWAIT, 0, 0],
    });
    client.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    client.extend(sleeps(30));

    let judged = judge(vec![idle_server(), NullProgram::single(client)], two_nodes());
    assert_eq!(answers(&judged, 1, RECVFROM), vec![EAGAIN]);
    assert!(marked_at(&judged, 1).is_some());
}

/// A receive asked for everything waits until all of it came.
///
/// The client sends 5 bytes, waits, sends 5 more; a receive of 10 with `MSG_WAITALL` returns after
/// the second.
#[test]
fn a_receive_asked_for_everything_waits_for_all_of_it() {
    let mut server = accepting();
    server.push(GuestOp::Syscall {
        nr: RECVFROM,
        args: [ACCEPTED_FD, INBOX_AT, 10, MSG_WAITALL, 0, 0],
    });
    server.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    server.extend(sleeps(30));

    let mut client = connecting(INET_STREAM);
    client.push(GuestOp::Syscall { nr: WRITE, args: [CLIENT_FD, PAYLOAD_AT, 5, 0, 0, 0] });
    client.extend(sleeps(4));
    client.push(GuestOp::Syscall { nr: WRITE, args: [CLIENT_FD, TAIL_AT, 5, 0, 0, 0] });
    client.extend(sleeps(30));

    let judged = judge(vec![NullProgram::single(server), NullProgram::single(client)], two_nodes());
    let second = judged
        .events
        .iter()
        .filter(|e| e.node == NodeId(1) && matches!(e.kind, EventKind::Syscall { nr: WRITE, .. }))
        .nth(1)
        .map(|e| e.tick)
        .expect("the client wrote twice");
    let returned = marked_at(&judged, 0).expect("the receive never returned");
    assert!(returned >= second, "returned at {returned:?}, before the rest came at {second:?}");
}

/// A datagram peek leaves the datagram queued, a truncating receive answers its whole length, and
/// a receive asked not to wait answers `EAGAIN` once the queue is empty.
#[test]
fn a_datagram_receive_honours_its_flags() {
    const UDP_FD: u64 = 3;
    let mut ops = vec![
        GuestOp::Syscall { nr: SOCKET, args: INET_DATAGRAM },
        GuestOp::Syscall { nr: BIND, args: [UDP_FD, SOCKADDR_AT, 16, 0, 0, 0] },
        GuestOp::Syscall { nr: SENDTO, args: [UDP_FD, PAYLOAD_AT, 5, 0, SOCKADDR_AT, 16] },
    ];
    ops.extend(sleeps(10));
    ops.push(GuestOp::Syscall { nr: RECVFROM, args: [UDP_FD, INBOX_AT, 64, MSG_PEEK, 0, 0] });
    ops.push(GuestOp::Syscall { nr: RECVFROM, args: [UDP_FD, INBOX_AT, 2, MSG_TRUNC, 0, 0] });
    ops.push(GuestOp::Syscall { nr: RECVFROM, args: [UDP_FD, INBOX_AT, 64, MSG_DONTWAIT, 0, 0] });
    ops.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    ops.extend(sleeps(30));

    let one = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        ..Scenario::default()
    };
    let judged = judge(vec![NullProgram::single(ops)], one);
    assert_eq!(answers(&judged, 0, RECVFROM), vec![5, 5, EAGAIN]);
    assert!(marked_at(&judged, 0).is_some());
}

/// A node reaches itself in a ring, as over loopback; only links between nodes follow the
/// topology.
#[test]
fn a_node_in_a_ring_reaches_itself() {
    use bohr_world::Topology;
    const UDP_FD: u64 = 3;
    let mut ops = vec![
        GuestOp::Syscall { nr: SOCKET, args: INET_DATAGRAM },
        GuestOp::Syscall { nr: BIND, args: [UDP_FD, SOCKADDR_AT, 16, 0, 0, 0] },
        GuestOp::Syscall { nr: SENDTO, args: [UDP_FD, PAYLOAD_AT, 5, 0, SOCKADDR_AT, 16] },
    ];
    ops.extend(sleeps(10));
    ops.push(GuestOp::Syscall { nr: RECVFROM, args: [UDP_FD, INBOX_AT, 64, MSG_DONTWAIT, 0, 0] });
    ops.extend(sleeps(30));
    let ring = Scenario {
        network: NetworkConfig { nodes: 4, topology: Topology::Ring, ..NetworkConfig::default() },
        ..Scenario::default()
    };
    let others = || NullProgram::single(sleeps(30));
    let judged = judge(vec![NullProgram::single(ops), others(), others(), others()], ring);
    assert_eq!(answers(&judged, 0, RECVFROM), vec![5], "the datagram to itself was lost");
}

/// Two partitions over one link: the first heal leaves the link cut by the second.
#[test]
fn a_link_two_partitions_cut_stays_cut_until_both_heal() {
    const SECOND_HEALS_AT: Tick = Tick(20 * STEP.get());
    let both_ways = vec![(NodeId(1), NodeId(0)), (NodeId(0), NodeId(1))];
    let overlapping = Scenario {
        network: NetworkConfig {
            nodes: 2,
            partitions: vec![
                ScheduledPartition {
                    at_tick: CUT_AT,
                    heal_at_tick: HEAL_AT,
                    cut: both_ways.clone(),
                },
                ScheduledPartition {
                    at_tick: Tick(6 * STEP.get()),
                    heal_at_tick: SECOND_HEALS_AT,
                    cut: both_ways,
                },
            ],
            ..NetworkConfig::default()
        },
        ..Scenario::default()
    };
    // The write goes after the first heal and before the second.
    let held = read_returned_at(&judge(vec![server(), client(10)], overlapping))
        .expect("a write held by the cut arrives once the last cut lifts");
    assert!(held >= SECOND_HEALS_AT, "read at {held:?}, while the second cut still held");
}

/// `SO_RCVTIMEO` ends a read nothing answers: the reader goes on after the timeout.
#[test]
fn a_receive_timeout_ends_the_wait() {
    const SETSOCKOPT: u32 = 54;
    const SOL_SOCKET: u64 = 1;
    const SO_RCVTIMEO: u64 = 20;
    let mut client = connecting(INET_STREAM);
    client.push(GuestOp::Syscall {
        nr: SETSOCKOPT,
        args: [CLIENT_FD, SOL_SOCKET, SO_RCVTIMEO, TIMEVAL_AT, 16, 0],
    });
    client.push(GuestOp::Syscall { nr: READ, args: [CLIENT_FD, INBOX_AT, 64, 0, 0, 0] });
    client.push(GuestOp::Syscall { nr: MARKER, args: [0; 6] });
    client.extend(sleeps(30));
    let judged = judge(vec![idle_server(), NullProgram::single(client)], two_nodes());

    let read_at = judged
        .events
        .iter()
        .find(|e| e.node == NodeId(1) && matches!(e.kind, EventKind::Syscall { nr: READ, .. }))
        .map(|e| e.tick)
        .expect("the client read");
    let went_on = marked_at(&judged, 1).expect("the read was never released");
    assert!(went_on.get() >= read_at.get() + 3 * STEP.get(), "released at {went_on:?}, early");
}

/// A non-blocking connect across a cut to a port nobody listens on is refused at the heal, and
/// `SO_ERROR` says so, as a runtime asks it.
#[test]
fn a_connect_refused_at_the_heal_is_told_by_so_error() {
    const GETSOCKOPT: u32 = 55;
    const SOL_SOCKET: u64 = 1;
    const SO_ERROR: u64 = 4;
    const EINPROGRESS: i64 = -115;
    const ECONNREFUSED: i32 = 111;
    let cut = Scenario {
        network: NetworkConfig {
            nodes: 2,
            partitions: vec![ScheduledPartition {
                at_tick: STEP,
                heal_at_tick: HEAL_AT,
                cut: vec![(NodeId(1), NodeId(0)), (NodeId(0), NodeId(1))],
            }],
            ..NetworkConfig::default()
        },
        ..Scenario::default()
    };
    let mut client = connecting(INET_STREAM_NONBLOCK);
    client.extend(sleeps(12));
    client.push(GuestOp::Syscall {
        nr: GETSOCKOPT,
        args: [CLIENT_FD, SOL_SOCKET, SO_ERROR, OPT_AT, OPT_LEN_AT, 0],
    });
    client.push(GuestOp::Syscall { nr: WRITE, args: [1, OPT_AT, 4, 0, 0, 0] });
    client.extend(sleeps(20));
    // Nobody listens on node 0.
    let judged = judge(vec![NullProgram::single(sleeps(40)), NullProgram::single(client)], cut);

    assert_eq!(answers(&judged, 1, CONNECT), vec![EINPROGRESS]);
    let told = judged.events.iter().find_map(|e| match e.kind {
        EventKind::Syscall { nr: WRITE, data_digest, .. } if e.node == NodeId(1) => data_digest,
        _ => None,
    });
    assert_eq!(told, Some(bohr_trace::short_digest(&ECONNREFUSED.to_le_bytes())));
}
