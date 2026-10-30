//! A key-value service in front of `redb`, and a client that holds it to what it answered.
//!
//! The store is not instrumented and knows nothing about any of this. What is written here is a
//! client, and the only claim it makes is the one every client makes: **a write the store
//! acknowledged is a write the store still has.** `commit` is what the acknowledgement rests on,
//! so a record that is missing after a crash is `commit` not having meant what it says.
//!
//! Two roles in one binary, chosen by the first argument, because both run from the same scenario
//! and a second crate would be a second thing to build.
//!
//! The transport is the plainest thing that works: blocking `sendto` on a datagram socket, one
//! port per node, addresses handed out by the world. Nothing here is a proposal about how to write
//! a network service; it exists so that "the store answered" is a moment the world can see.

use std::{
    io::ErrorKind,
    net::{SocketAddrV4, UdpSocket},
    time::Duration,
};

use redb::{Database, TableDefinition};

/// Where the store keeps its database, on the node's own filesystem.
const STORE_PATH: &str = "/store.redb";

/// The one table. A hunt is about durability, not about schema.
const RECORDS: TableDefinition<u64, &[u8]> = TableDefinition::new("records");

/// Port both roles use. Each node has an address of its own, so one port is enough.
const PORT: u16 = 7000;

/// Bytes stored per record. Large enough that a record spans more than a header field and small
/// enough that a run of them is not the thing being measured.
const RECORD_BYTES: usize = 256;

/// Largest datagram either role sends.
const MAX_DATAGRAM: usize = 512;

/// How many records the client offers.
const RECORDS_TO_WRITE: u64 = 64;

/// Records a prepared database already holds when the run begins.
///
/// An empty store never reaches the code a full one lives in: a region grown past its first, pages
/// freed and reused, a savepoint ring with something in it. Recovery from a large database is not
/// the same path as recovery from a small one, and the second is the one every run took until now.
const PREPARED_RECORDS: u64 = 2_000;

/// Where a prepared record's key sits, clear of anything the client writes.
///
/// Distinct so the two claims stay separate: what the client was told about is one question, and
/// what was already durable before the run began is another.
const PREPARED_BASE: u64 = 1_000_000;

/// How many prepared records the client checks after the run has had its crashes.
///
/// A sample rather than all of them, because the claim is about the store and not about our
/// patience: a store that lost one of two thousand loses them in runs, not in ones.
const PREPARED_SAMPLE: u64 = 32;

/// How many polls the client gives one answer before treating it as unanswered.
///
/// A bound rather than a socket timeout: the store may be dead when the question is asked, and a
/// client that blocked forever on a dead peer would report a deadlock where the interesting fact
/// is that the peer is dead. The product of this and the interval below has to cover the longest
/// the store is ever down for, or every question asked during a restart goes unanswered.
const POLLS_PER_ANSWER: u32 = 40;

/// How long the client waits between polls, in virtual time.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// How long the store serves before ending.
///
/// A service normally runs until something stops it, and a guest that never ends cannot be judged:
/// with the client gone and no timer armed, every task is waiting and the run is a deadlock. The
/// bound is virtual time, which the world answers, so it costs the run nothing real.
const STORE_LIFETIME: Duration = Duration::from_secs(6);

/// The address the world gives node `index`.
fn address_of(index: u64) -> SocketAddrV4 {
    SocketAddrV4::new([10, 66, 0, (index + 1) as u8].into(), PORT)
}

/// The node index decides the role, because a scenario gives every node the same argument list and
/// substitutes only the index into it. Node zero owns the database; every other node is a client.
fn main() {
    let argument = std::env::args().nth(1).unwrap_or_default();
    let code = match argument.as_str() {
        "solo" => solo(),
        "prepare" => prepare(&std::env::args().nth(2).unwrap_or_default()),
        index => match index.parse::<u64>() {
            Ok(0) => store(0),
            Ok(index) => client(index),
            Err(_) => {
                eprintln!("expected a node index or `solo`, got {index}");
                2
            }
        },
    };
    std::process::exit(code);
}

/// Lays down the database a run starts from, on the host and outside any supervisor.
///
/// Run once, by hand or by a script, and the directory it writes is what `[state] dir` names. It
/// is deliberately not part of a run: a prepared state is an input identified by digest, and a run
/// that built its own would be measuring the building.
fn prepare(dir: &str) -> i32 {
    if dir.is_empty() {
        eprintln!("usage: redb-hunt prepare <directory>");
        return 2;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("cannot make {dir}: {e}");
        return 2;
    }
    let path = std::path::Path::new(dir).join("store.redb");
    let _ = std::fs::remove_file(&path);

    let db = match Database::create(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot create {}: {e}", path.display());
            return 2;
        }
    };
    // One transaction rather than two thousand: what is wanted is a database with something in it,
    // and the shape of how it got there is the run's business rather than the fixture's.
    let value = vec![b'p'; RECORD_BYTES];
    let write = match db.begin_write() {
        Ok(write) => write,
        Err(e) => {
            eprintln!("cannot begin: {e}");
            return 2;
        }
    };
    {
        let mut table = match write.open_table(RECORDS) {
            Ok(table) => table,
            Err(e) => {
                eprintln!("cannot open the table: {e}");
                return 2;
            }
        };
        for i in 0..PREPARED_RECORDS {
            if let Err(e) = table.insert(PREPARED_BASE + i, value.as_slice()) {
                eprintln!("cannot insert: {e}");
                return 2;
            }
        }
    }
    if let Err(e) = write.commit() {
        eprintln!("cannot commit: {e}");
        return 2;
    }
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("prepared {PREPARED_RECORDS} records in {} ({bytes} bytes)", path.display());
    0
}

/// Writes and reads back without a network, which is the smoke test that the store runs at all.
fn solo() -> i32 {
    let db = match Database::create(STORE_PATH) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot create {STORE_PATH}: {e}");
            return 2;
        }
    };
    for key in 0..RECORDS_TO_WRITE {
        if let Err(e) = commit_record(&db, key) {
            eprintln!("write {key} failed: {e}");
            return 2;
        }
    }
    for key in 0..RECORDS_TO_WRITE {
        match holds(&db, key) {
            Ok(true) => {}
            Ok(false) => {
                println!("record {key} is missing");
                return 1;
            }
            Err(e) => {
                eprintln!("read {key} failed: {e}");
                return 2;
            }
        }
    }
    println!("wrote and read back {RECORDS_TO_WRITE} records");
    0
}

/// Answers requests from the database, acknowledging a write only after `commit` returns.
///
/// The order is the whole point: the acknowledgement leaves after `commit` has returned, so the
/// store has been asked for durability before the client is told it has it.
fn store(index: u64) -> i32 {
    let db = match Database::create(STORE_PATH) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot open {STORE_PATH}: {e}");
            return 2;
        }
    };
    let socket = match UdpSocket::bind(address_of(index)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot bind: {e}");
            return 2;
        }
    };

    if socket.set_nonblocking(true).is_err() {
        eprintln!("cannot make the socket non-blocking");
        return 2;
    }

    let started = std::time::Instant::now();
    let mut buf = [0u8; MAX_DATAGRAM];
    while started.elapsed() < STORE_LIFETIME {
        let (len, from) = match socket.recv_from(&mut buf) {
            Ok(received) => received,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(_) => continue,
        };
        let request = String::from_utf8_lossy(&buf[..len]).into_owned();
        let mut words = request.split_whitespace();
        let verb = words.next().unwrap_or_default();
        let Some(key) = words.next().and_then(|k| k.parse::<u64>().ok()) else {
            continue;
        };

        let answer = match verb {
            "put" => match commit_record(&db, key) {
                Ok(()) => format!("ack {key}"),
                Err(e) => format!("err {key} {e}"),
            },
            "get" => match holds(&db, key) {
                Ok(true) => format!("have {key}"),
                Ok(false) => format!("missing {key}"),
                Err(e) => format!("err {key} {e}"),
            },
            // A removal is a promise like any other: what the store said it took away must stay
            // away, and a crash that brings it back is a resurrection rather than a loss.
            "drop" => match drop_record(&db, key) {
                Ok(()) => format!("gone {key}"),
                Err(e) => format!("err {key} {e}"),
            },
            "done" => {
                return 0;
            }
            _ => continue,
        };
        let _ = socket.send_to(answer.as_bytes(), from);
    }
    0
}

/// Offers records, remembers what was acknowledged, and asks for it back.
///
/// The verdict is entirely about what the store said: a record the store never acknowledged is
/// nobody's fault and is not asked about. Only an acknowledged record that comes back missing is a
/// finding, and it is a finding whatever the store did in between.
fn client(index: u64) -> i32 {
    let socket = match UdpSocket::bind(address_of(index)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot bind: {e}");
            return 2;
        }
    };
    if socket.set_nonblocking(true).is_err() {
        eprintln!("cannot make the socket non-blocking");
        return 2;
    }
    let store = address_of(0);

    // Which prepared records the store actually had before anything happened. Only these are
    // asked for at the end: a scenario that prepared no state has none of them, and holding a
    // store to records nobody ever wrote would report a finding on every run of every empty
    // database — a client must hold a store to what it observed and to nothing else.
    let prepared: Vec<u64> = (0..PREPARED_SAMPLE)
        .map(|i| PREPARED_BASE + i * (PREPARED_RECORDS / PREPARED_SAMPLE).max(1))
        .filter(|key| {
            matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("have "))
        })
        .collect();

    let mut acknowledged: Vec<u64> = Vec::new();
    // Offers the store refused. A refusal is a promise not made, so nothing is owed — but a
    // refused write that is there afterwards is the store having said no and done yes, and
    // nothing else in the run would notice.
    let mut refused: Vec<u64> = Vec::new();
    for key in 0..RECORDS_TO_WRITE {
        match ask(&socket, store, &format!("put {key}")) {
            Some(answer) if answer.starts_with("ack ") => acknowledged.push(key),
            Some(answer) if answer.starts_with("err ") => refused.push(key),
            // Silence is neither: the store may have applied the write and died before saying so.
            _ => {}
        }
    }

    let mut verified = 0usize;
    let mut lost = Vec::new();
    for key in &acknowledged {
        match ask(&socket, store, &format!("get {key}")) {
            Some(answer) if answer.starts_with("have ") => verified += 1,
            Some(answer) if answer.starts_with("missing ") => lost.push(*key),
            // No answer at all is the store being dead or unreachable, which is the world doing
            // what it was told rather than the store breaking a promise. Counted, never inferred
            // from: a question that was not answered is not a question that was answered well.
            _ => {}
        }
    }

    // What the store took away must stay away. Half of the acknowledged records are removed and
    // asked about again: a crash that brings one back is a resurrection, and it is a defect of the
    // same family as a loss — the store answered, and what it answered is not what it holds.
    let mut resurrected_after_drop = Vec::new();
    for key in acknowledged.iter().step_by(2) {
        if matches!(ask(&socket, store, &format!("drop {key}")), Some(a) if a.starts_with("gone ")) {
            if matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("have "))
            {
                resurrected_after_drop.push(*key);
            }
        }
    }

    // Records that were durable before the run began. Nothing this run did was about them, so a
    // store that lost one lost data no crash in this run had any business touching.
    let mut prepared_lost = Vec::new();
    for key in &prepared {
        if matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("missing "))
        {
            prepared_lost.push(*key);
        }
    }

    // A write the store refused and has anyway. Reported beside the verdict rather than as one,
    // because whether it is a defect depends on what the refusal meant: a commit that failed
    // during its durability step may legitimately have the data in memory, and only a crash
    // decides. Counted here so the question can be asked of real data instead of guessed at.
    let mut resurrected = Vec::new();
    for key in &refused {
        if matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("have ")) {
            resurrected.push(*key);
        }
    }

    let _ = socket.send_to(b"done 0", store);

    // One line, always, carrying every number. A run whose client said nothing is a run that
    // checked nothing, and it must not be possible to mistake it for a run that passed — which is
    // exactly what a summary printed only on success would allow.
    // Counts first and the keys last, so a reader of a campaign can take the numbers by name
    // without a list in the middle deciding where the next number begins.
    println!(
        "acknowledged {} verified {} lost {} refused {} resurrected {} undropped {} \
         prepared_lost {} · lost={lost:?} resurrected={resurrected:?} \
         undropped={resurrected_after_drop:?} prepared={prepared_lost:?}",
        acknowledged.len(),
        verified,
        lost.len(),
        refused.len(),
        resurrected.len(),
        resurrected_after_drop.len(),
        prepared_lost.len()
    );

    // Three claims, and a run is a finding if it broke any of them: a record the store said it had
    // and lost, one it said it removed and kept, and one that was durable before the run began.
    if lost.is_empty() && resurrected_after_drop.is_empty() && prepared_lost.is_empty() {
        return 0;
    }

    // Aborted rather than returned, and the difference decides whether the finding is seen at all.
    // A run's exit status is the last task's, and this client is not the last: the store outlives
    // it and ends cleanly, so a status of one is overwritten by a status of zero and the run reads
    // as clean. A signal is not overwritten — it is what the oracle looks at first.
    std::io::Write::flush(&mut std::io::stdout()).ok();
    std::process::abort();
}

/// Sends one request and polls for its answer, or gives up.
fn ask(socket: &UdpSocket, to: SocketAddrV4, request: &str) -> Option<String> {
    if socket.send_to(request.as_bytes(), to).is_err() {
        return None;
    }
    let mut buf = [0u8; MAX_DATAGRAM];
    for _ in 0..POLLS_PER_ANSWER {
        match socket.recv_from(&mut buf) {
            Ok((len, _)) => return Some(String::from_utf8_lossy(&buf[..len]).into_owned()),
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL_INTERVAL),
            Err(_) => return None,
        }
    }
    None
}

/// One record, taken away and committed.
fn drop_record(db: &Database, key: u64) -> Result<(), redb::Error> {
    let write = db.begin_write()?;
    {
        let mut table = write.open_table(RECORDS)?;
        table.remove(key)?;
    }
    write.commit()?;
    Ok(())
}

/// One record, written and committed.
fn commit_record(db: &Database, key: u64) -> Result<(), redb::Error> {
    let value = vec![b'r'; RECORD_BYTES];
    let write = db.begin_write()?;
    {
        let mut table = write.open_table(RECORDS)?;
        table.insert(key, value.as_slice())?;
    }
    write.commit()?;
    Ok(())
}

/// Whether the store still holds a record.
fn holds(db: &Database, key: u64) -> Result<bool, redb::Error> {
    let read = db.begin_read()?;
    let table = match read.open_table(RECORDS) {
        Ok(table) => table,
        // A table that does not exist holds nothing, which is a legitimate answer to the question
        // and not an error: a crash before the first commit leaves exactly this.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(table.get(key)?.is_some())
}
