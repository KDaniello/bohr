//! One process has one table of descriptor numbers, whatever each number names.
//!
//! A socket, a poller and a counter take the lowest free number, as a file does, and give it back
//! when closed. A copy names the same socket and shares its flags; the socket lives while any copy
//! does. `fcntl` reads and sets a socket's flags as Linux does.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::EventKind;
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::NetworkConfig;

const READ: u32 = 0;
const WRITE: u32 = 1;
const CLOSE: u32 = 3;
const DUP2: u32 = 33;
const SOCKET: u32 = 41;
const BIND: u32 = 49;
const LISTEN: u32 = 50;
const SOCKETPAIR: u32 = 53;
const FCNTL: u32 = 72;
const OPENAT: u32 = 257;
const EVENTFD2: u32 = 290;
const EPOLL_CREATE1: u32 = 291;
const ACCEPT4: u32 = 288;

const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const O_RDWR: i64 = 2;
const O_NONBLOCK: u64 = 0o4000;
const O_CREAT: u64 = 0o100;
const SOCK_NONBLOCK: u64 = 0o4000;
const EAGAIN: i64 = -11;

/// `AF_UNIX`, `SOCK_STREAM`; and `AF_INET`, `SOCK_STREAM`.
const UNIX_STREAM: u64 = 1;
const INET: u64 = 2;
const STREAM: u64 = 1;

const PATH_AT: u64 = MEMORY_BASE;
const SOCKADDR_AT: u64 = MEMORY_BASE + 0x100;
const PAIR_AT: u64 = MEMORY_BASE + 0x200;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x300;
const INBOX_AT: u64 = MEMORY_BASE + 0x400;

fn memory() -> Vec<bohr_testkit::Preload> {
    // `sockaddr_in` for node 0: AF_INET, port 7000, 10.66.0.1.
    let mut address = vec![2, 0, 0x1b, 0x58, 10, 66, 0, 1];
    address.extend_from_slice(&[0; 8]);
    vec![
        (0, PATH_AT, b"/tmp/file\0".to_vec()),
        (0, SOCKADDR_AT, address),
        (0, PAYLOAD_AT, b"hello".to_vec()),
    ]
}

/// One node running `ops`, and what it was answered.
fn run(ops: Vec<GuestOp>) -> Judged {
    let mut ops = ops;
    ops.push(GuestOp::Exit(0));
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        ..Scenario::default()
    };
    let config = RunConfig { max_ticks: Tick(Tick::SECOND.get()), ..RunConfig::default() };
    judge_scenario(
        vec![NullProgram::single(ops)],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(1)),
        config,
    )
}

fn call(nr: u32, args: [u64; 6]) -> GuestOp {
    GuestOp::Syscall { nr, args }
}

/// Every answer, in order.
fn answers(judged: &Judged) -> Vec<i64> {
    judged
        .events
        .iter()
        .filter(|e| e.node == NodeId(0))
        .filter_map(|e| match e.kind {
            EventKind::Syscall { ret, .. } => Some(ret),
            _ => None,
        })
        .collect()
}

/// A socket, a file, a poller and a counter share one table, and a closed number is taken again.
#[test]
fn every_kind_of_descriptor_takes_the_lowest_free_number() {
    let judged = run(vec![
        call(SOCKET, [INET, STREAM, 0, 0, 0, 0]),
        call(OPENAT, [u64::MAX - 99, PATH_AT, O_CREAT, 0o644, 0, 0]),
        call(CLOSE, [3, 0, 0, 0, 0, 0]),
        call(EPOLL_CREATE1, [0; 6]),
        call(EVENTFD2, [0; 6]),
    ]);
    assert_eq!(answers(&judged), vec![3, 4, 0, 3, 5]);
}

/// `dup2` copies a socket onto a standard number, and a write through the copy reaches the peer.
#[test]
fn a_socket_copied_onto_a_standard_number_carries_the_stream() {
    let judged = run(vec![
        call(SOCKETPAIR, [UNIX_STREAM, STREAM, 0, PAIR_AT, 0, 0]),
        call(DUP2, [3, 0, 0, 0, 0, 0]),
        call(WRITE, [0, PAYLOAD_AT, 5, 0, 0, 0]),
        call(READ, [4, INBOX_AT, 64, 0, 0, 0]),
    ]);
    assert_eq!(answers(&judged), vec![0, 0, 5, 5]);
}

/// Closing the number a socket was opened under leaves its copy working, and the peer hears no
/// end.
#[test]
fn a_copy_outlives_the_number_it_was_copied_from() {
    let judged = run(vec![
        call(SOCKETPAIR, [UNIX_STREAM, STREAM, 0, PAIR_AT, 0, 0]),
        call(FCNTL, [3, F_DUPFD, 10, 0, 0, 0]),
        call(CLOSE, [3, 0, 0, 0, 0, 0]),
        call(WRITE, [10, PAYLOAD_AT, 5, 0, 0, 0]),
        call(READ, [4, INBOX_AT, 64, 0, 0, 0]),
    ]);
    assert_eq!(answers(&judged), vec![0, 10, 0, 5, 5]);
}

/// `F_SETFL` makes a listener non-blocking, `F_GETFL` reads it back, and `accept` no longer waits.
#[test]
fn a_listener_made_non_blocking_by_fcntl_does_not_wait() {
    let judged = run(vec![
        call(SOCKET, [INET, STREAM, 0, 0, 0, 0]),
        call(BIND, [3, SOCKADDR_AT, 16, 0, 0, 0]),
        call(LISTEN, [3, 16, 0, 0, 0, 0]),
        call(FCNTL, [3, F_GETFL, 0, 0, 0, 0]),
        call(FCNTL, [3, F_SETFL, O_NONBLOCK, 0, 0, 0]),
        call(FCNTL, [3, F_GETFL, 0, 0, 0, 0]),
        call(ACCEPT4, [3, 0, 0, 0, 0, 0]),
    ]);
    assert_eq!(answers(&judged), vec![3, 0, 0, O_RDWR, 0, O_RDWR | O_NONBLOCK as i64, EAGAIN]);
}

/// A pair opened with `SOCK_NONBLOCK` answers an empty read with `EAGAIN`.
#[test]
fn a_pair_opened_non_blocking_does_not_wait() {
    let judged = run(vec![
        call(SOCKETPAIR, [UNIX_STREAM, STREAM | SOCK_NONBLOCK, 0, PAIR_AT, 0, 0]),
        call(READ, [3, INBOX_AT, 64, 0, 0, 0]),
    ]);
    assert_eq!(answers(&judged), vec![0, EAGAIN]);
}

/// A file reports the flags it was opened with, and `F_SETFL` changes the ones Linux lets it.
#[test]
fn a_file_reports_and_changes_its_status_flags() {
    const O_WRONLY: u64 = 1;
    const O_APPEND: u64 = 0o2000;
    const O_LARGEFILE: i64 = 0o100_000;
    let judged = run(vec![
        call(OPENAT, [u64::MAX - 99, PATH_AT, O_CREAT | O_WRONLY | O_APPEND, 0o644, 0, 0]),
        call(FCNTL, [3, F_GETFL, 0, 0, 0, 0]),
        call(FCNTL, [3, F_SETFL, O_NONBLOCK, 0, 0, 0]),
        call(FCNTL, [3, F_GETFL, 0, 0, 0, 0]),
        call(FCNTL, [1, F_GETFL, 0, 0, 0, 0]),
        call(FCNTL, [3, 9999, 0, 0, 0, 0]),
    ]);
    let appending = O_WRONLY as i64 | O_APPEND as i64 | O_LARGEFILE;
    let blocking_no_more = O_WRONLY as i64 | O_NONBLOCK as i64 | O_LARGEFILE;
    assert_eq!(
        answers(&judged),
        vec![3, appending, 0, blocking_no_more, O_RDWR | O_LARGEFILE, -22],
        "open flags, F_SETFL replacing O_APPEND with O_NONBLOCK, stdout, an unknown command"
    );
}

/// `F_GETLK` writes `F_UNLCK`: one process's own locks never stand in its way.
///
/// The `struct flock` asks about a write lock; what comes back is written to standard output, whose
/// digest the trace keeps.
#[test]
fn a_lock_query_finds_nothing_in_the_way() {
    const F_GETLK: u64 = 5;
    const F_SETLK: u64 = 6;
    const F_WRLCK: i16 = 1;
    const F_UNLCK: i16 = 2;
    const FLOCK_AT: u64 = MEMORY_BASE + 0x500;
    let mut ops = vec![
        call(OPENAT, [u64::MAX - 99, PATH_AT, O_CREAT | 2, 0o644, 0, 0]),
        call(FCNTL, [3, F_SETLK, FLOCK_AT, 0, 0, 0]),
        call(FCNTL, [3, F_GETLK, FLOCK_AT, 0, 0, 0]),
        call(WRITE, [1, FLOCK_AT, 2, 0, 0, 0]),
    ];
    ops.push(GuestOp::Exit(0));
    let mut memory = memory();
    let mut flock = F_WRLCK.to_le_bytes().to_vec();
    flock.resize(32, 0);
    memory.push((0, FLOCK_AT, flock));
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        ..Scenario::default()
    };
    let config = RunConfig { max_ticks: Tick(Tick::SECOND.get()), ..RunConfig::default() };
    let judged = judge_scenario(
        vec![NullProgram::single(ops)],
        &memory,
        scenario,
        &Tape::from_seed(Seed(1)),
        config,
    );
    assert_eq!(answers(&judged), vec![3, 0, 0, 2]);
    let written = judged.events.iter().find_map(|e| match e.kind {
        EventKind::Syscall { nr: WRITE, data_digest, .. } => data_digest,
        _ => None,
    });
    assert_eq!(
        written,
        Some(bohr_trace::short_digest(&F_UNLCK.to_le_bytes())),
        "the lock type read back is not F_UNLCK"
    );
}

/// `FD_CLOEXEC` is kept per number, and `F_DUPFD` honours its minimum for a file too.
#[test]
fn descriptor_flags_and_minimums_are_kept() {
    let judged = run(vec![
        call(SOCKET, [INET, STREAM, 0, 0, 0, 0]),
        call(FCNTL, [3, F_SETFD, 1, 0, 0, 0]),
        call(FCNTL, [3, F_GETFD, 0, 0, 0, 0]),
        call(OPENAT, [u64::MAX - 99, PATH_AT, O_CREAT, 0o644, 0, 0]),
        call(FCNTL, [4, F_DUPFD, 10, 0, 0, 0]),
    ]);
    assert_eq!(answers(&judged), vec![3, 0, 1, 4, 10]);
}

/// A pair of `SOCK_DGRAM` keeps each write a message: two writes are two reads, and what does not
/// fit a read is lost with its message.
#[test]
fn a_datagram_pair_keeps_its_messages_apart() {
    const DGRAM: u64 = 2;
    let judged = run(vec![
        call(SOCKETPAIR, [UNIX_STREAM, DGRAM, 0, PAIR_AT, 0, 0]),
        call(WRITE, [3, PAYLOAD_AT, 5, 0, 0, 0]),
        call(WRITE, [3, PAYLOAD_AT, 5, 0, 0, 0]),
        call(WRITE, [3, PAYLOAD_AT, 5, 0, 0, 0]),
        call(READ, [4, INBOX_AT, 64, 0, 0, 0]),
        call(READ, [4, INBOX_AT, 2, 0, 0, 0]),
        call(READ, [4, INBOX_AT, 64, 0, 0, 0]),
    ]);
    assert_eq!(answers(&judged), vec![0, 5, 5, 5, 5, 2, 5]);
}

/// A semaphore counter gives one per read; a non-blocking one at nought answers `EAGAIN`.
#[test]
fn a_semaphore_counter_gives_one_per_read() {
    const EFD_SEMAPHORE: u64 = 1;
    let judged = run(vec![
        call(EVENTFD2, [3, EFD_SEMAPHORE | O_NONBLOCK, 0, 0, 0, 0]),
        call(READ, [3, INBOX_AT, 8, 0, 0, 0]),
        call(READ, [3, INBOX_AT, 8, 0, 0, 0]),
        call(READ, [3, INBOX_AT, 8, 0, 0, 0]),
        call(READ, [3, INBOX_AT, 8, 0, 0, 0]),
    ]);
    assert_eq!(answers(&judged), vec![3, 8, 8, 8, EAGAIN]);
}

/// A blocking counter at nought waits for a write instead of answering `EAGAIN`.
#[test]
fn a_blocking_counter_at_nought_waits() {
    const GETPID: u32 = 39;
    let judged = run(vec![
        call(EVENTFD2, [0, 0, 0, 0, 0, 0]),
        call(READ, [3, INBOX_AT, 8, 0, 0, 0]),
        call(GETPID, [0; 6]),
    ]);
    let waited =
        !judged.events.iter().any(|e| matches!(e.kind, EventKind::Syscall { nr: GETPID, .. }));
    assert!(waited, "a read of a blocking counter at nought returned: {:?}", answers(&judged));
}

/// `epoll_wait`'s timeout is an `int`: -1 in the low half waits for ever, whatever the upper half
/// holds, as Go's assembly passes it.
#[test]
fn an_epoll_wait_of_minus_one_waits_for_ever() {
    const EPOLL_WAIT: u32 = 232;
    const GETPID: u32 = 39;
    const SIXTY_DAYS: Tick = Tick(60 * 24 * 3600 * Tick::SECOND.get());
    let ops = vec![
        call(EPOLL_CREATE1, [0; 6]),
        call(EPOLL_WAIT, [3, INBOX_AT, 1, 0xFFFF_FFFF, 0, 0]),
        call(GETPID, [0; 6]),
        GuestOp::Exit(0),
    ];
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        ..Scenario::default()
    };
    let config =
        RunConfig { horizon: Some(SIXTY_DAYS), max_ticks: SIXTY_DAYS, ..RunConfig::default() };
    let judged = judge_scenario(
        vec![NullProgram::single(ops)],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(1)),
        config,
    );
    let returned =
        judged.events.iter().any(|e| matches!(e.kind, EventKind::Syscall { nr: GETPID, .. }));
    assert!(!returned, "an epoll_wait of -1 timed out: {:?}", answers(&judged));
}

/// `getsockopt` reads what was set, Linux's default where nothing was, and refuses an option
/// nobody set and the model does not know.
#[test]
fn socket_options_read_back_what_linux_would_say() {
    const SETSOCKOPT: u32 = 54;
    const GETSOCKOPT: u32 = 55;
    const SOL_SOCKET: u64 = 1;
    const SO_RCVBUF: u64 = 8;
    const SO_KEEPALIVE: u64 = 9;
    const ENOPROTOOPT: i64 = -92;
    const OPT_AT: u64 = MEMORY_BASE + 0x600;
    const LEN_AT: u64 = MEMORY_BASE + 0x640;
    const ONE_AT: u64 = MEMORY_BASE + 0x680;
    let mut ops = vec![
        call(SOCKET, [INET, STREAM, 0, 0, 0, 0]),
        call(GETSOCKOPT, [3, SOL_SOCKET, SO_RCVBUF, OPT_AT, LEN_AT, 0]),
        call(WRITE, [1, OPT_AT, 4, 0, 0, 0]),
        call(SETSOCKOPT, [3, SOL_SOCKET, SO_KEEPALIVE, ONE_AT, 4, 0]),
        call(GETSOCKOPT, [3, SOL_SOCKET, SO_KEEPALIVE, OPT_AT, LEN_AT, 0]),
        call(WRITE, [1, OPT_AT, 4, 0, 0, 0]),
        call(GETSOCKOPT, [3, SOL_SOCKET, 999, OPT_AT, LEN_AT, 0]),
    ];
    ops.push(GuestOp::Exit(0));
    let mut memory = memory();
    memory.push((0, LEN_AT, 4u32.to_le_bytes().to_vec()));
    memory.push((0, ONE_AT, 1i32.to_le_bytes().to_vec()));
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        ..Scenario::default()
    };
    let config = RunConfig { max_ticks: Tick(Tick::SECOND.get()), ..RunConfig::default() };
    let judged = judge_scenario(
        vec![NullProgram::single(ops)],
        &memory,
        scenario,
        &Tape::from_seed(Seed(1)),
        config,
    );
    assert_eq!(answers(&judged), vec![3, 0, 4, 0, 0, 4, ENOPROTOOPT]);
    let written: Vec<[u8; 8]> = judged
        .events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Syscall { nr: WRITE, data_digest, .. } => data_digest,
            _ => None,
        })
        .collect();
    let digest = |value: i32| bohr_trace::short_digest(&value.to_le_bytes());
    assert_eq!(written, vec![digest(131_072), digest(1)], "SO_RCVBUF, then SO_KEEPALIVE as set");
}

/// An edge-triggered watch reports a readable stream once per arrival and a writable one once per
/// return of room, and a call on another socket is no edge.
#[test]
fn an_edge_triggered_watch_reports_edges_and_nothing_else() {
    const EPOLL_WAIT: u32 = 232;
    const EPOLL_CTL: u32 = 233;
    const CTL_ADD: u64 = 1;
    const EPOLLIN: u32 = 1;
    const EPOLLOUT: u32 = 4;
    const EPOLLET: u32 = 1 << 31;
    const IN_AT: u64 = MEMORY_BASE + 0x500;
    const OUT_AT: u64 = MEMORY_BASE + 0x520;
    let watch = |events: u32, token: u64| {
        let mut bytes = events.to_le_bytes().to_vec();
        bytes.extend_from_slice(&token.to_le_bytes());
        bytes
    };
    let wait = call(EPOLL_WAIT, [7, INBOX_AT, 4, 0, 0, 0]);
    let mut ops = vec![
        call(SOCKETPAIR, [UNIX_STREAM, STREAM, 0, PAIR_AT, 0, 0]),
        call(SOCKETPAIR, [UNIX_STREAM, STREAM, 0, PAIR_AT, 0, 0]),
        call(EPOLL_CREATE1, [0; 6]),
        call(EPOLL_CTL, [7, CTL_ADD, 3, IN_AT, 0, 0]),
        call(EPOLL_CTL, [7, CTL_ADD, 5, OUT_AT, 0, 0]),
        wait.clone(),
        call(WRITE, [4, PAYLOAD_AT, 5, 0, 0, 0]),
        wait.clone(),
        call(WRITE, [5, PAYLOAD_AT, 5, 0, 0, 0]),
        wait.clone(),
        call(WRITE, [4, PAYLOAD_AT, 5, 0, 0, 0]),
        wait,
    ];
    ops.push(GuestOp::Exit(0));
    let mut preloads = memory();
    preloads.push((0, IN_AT, watch(EPOLLIN | EPOLLET, 3)));
    preloads.push((0, OUT_AT, watch(EPOLLOUT | EPOLLET, 5)));
    let judged = judge_scenario(
        vec![NullProgram::single(ops)],
        &preloads,
        Scenario {
            network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
            ..Scenario::default()
        },
        &Tape::from_seed(Seed(1)),
        RunConfig { max_ticks: Tick(Tick::SECOND.get()), ..RunConfig::default() },
    );
    // The first wait finds the writable end; an arrival is one edge; a write on another end,
    // which went whole, is none; the next arrival is another.
    assert_eq!(answers(&judged), [0, 0, 7, 0, 0, 1, 5, 1, 5, 0, 5, 1]);
}
