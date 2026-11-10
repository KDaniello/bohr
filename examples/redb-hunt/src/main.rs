//! A key-value service in front of `redb`, and a client that holds it to what it answered.
//!
//! The store is not instrumented. The client claims only that an acknowledged write is still
//! there; acknowledgement follows `commit`.
//!
//! Two roles in one binary, chosen by node index.
//!
//! Blocking `sendto` on a datagram socket, so the world sees when the store answers.

use std::{
    io::ErrorKind,
    net::{SocketAddrV4, UdpSocket},
    time::Duration,
};

use redb::{Database, TableDefinition};

/// Where the store keeps its database, on the node's own filesystem.
const STORE_PATH: &str = "/store.redb";

/// The one table.
const RECORDS: TableDefinition<u64, &[u8]> = TableDefinition::new("records");

/// Port both roles use.
const PORT: u16 = 7000;

/// Bytes stored per record.
const RECORD_BYTES: usize = 256;

/// Largest datagram either role sends.
const MAX_DATAGRAM: usize = 512;

/// How many records the client offers.
const RECORDS_TO_WRITE: u64 = 64;

/// Records a prepared database already holds when the run begins.
///
/// A full store exercises grown regions, reused pages and savepoints, which an empty one does not.
const PREPARED_RECORDS: u64 = 2_000;

/// Where a prepared record's key sits, clear of anything the client writes.
///
/// Separate, so the two claims stay apart.
const PREPARED_BASE: u64 = 1_000_000;

/// How many prepared records the client checks after the run has had its crashes.
///
/// A sample, since losses come in runs.
const PREPARED_SAMPLE: u64 = 32;

/// How many polls the client gives one answer before treating it as unanswered.
///
/// Bounded polls, so a dead store is no deadlock. With the interval below it
/// must cover the longest outage.
const POLLS_PER_ANSWER: u32 = 40;

/// How long the client waits between polls, in virtual time.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// How long the store serves before ending.
///
/// Bounded in virtual time, so the run ends instead of deadlocking.
const STORE_LIFETIME: Duration = Duration::from_secs(6);

/// The address the world gives node `index`.
fn address_of(index: u64) -> SocketAddrV4 {
    SocketAddrV4::new([10, 66, 0, (index + 1) as u8].into(), PORT)
}

/// Node 0 owns the database; every other node is a client.
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
/// Run once, outside any run; `[state] dir` names the result.
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
    // One transaction.
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
/// The acknowledgement leaves only after `commit` returns.
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
            // A removal must stay removed.
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
/// Only an acknowledged record that comes back missing is a finding.
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

    // Prepared records seen at the start; only these are checked at the end.
    let prepared: Vec<u64> = (0..PREPARED_SAMPLE)
        .map(|i| PREPARED_BASE + i * (PREPARED_RECORDS / PREPARED_SAMPLE).max(1))
        .filter(|key| {
            matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("have "))
        })
        .collect();

    let mut acknowledged: Vec<u64> = Vec::new();
    // Refused offers, checked later for writes that landed anyway.
    let mut refused: Vec<u64> = Vec::new();
    for key in 0..RECORDS_TO_WRITE {
        match ask(&socket, store, &format!("put {key}")) {
            Some(answer) if answer.starts_with("ack ") => acknowledged.push(key),
            Some(answer) if answer.starts_with("err ") => refused.push(key),
            // Silence is neither acknowledgement nor refusal.
            _ => {}
        }
    }

    let mut verified = 0usize;
    let mut lost = Vec::new();
    for key in &acknowledged {
        match ask(&socket, store, &format!("get {key}")) {
            Some(answer) if answer.starts_with("have ") => verified += 1,
            Some(answer) if answer.starts_with("missing ") => lost.push(*key),
            // No answer: the store is dead or unreachable. Counted.
            _ => {}
        }
    }

    // Half the acknowledged records are removed and checked again for resurrection.
    let mut resurrected_after_drop = Vec::new();
    for key in acknowledged.iter().step_by(2) {
        if matches!(ask(&socket, store, &format!("drop {key}")), Some(a) if a.starts_with("gone ")) {
            if matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("have "))
            {
                resurrected_after_drop.push(*key);
            }
        }
    }

    // Records durable before the run; losing one is a finding.
    let mut prepared_lost = Vec::new();
    for key in &prepared {
        if matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("missing "))
        {
            prepared_lost.push(*key);
        }
    }

    // Refused but present: reported without a verdict, since a failed commit may keep data in memory.
    let mut resurrected = Vec::new();
    for key in &refused {
        if matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("have ")) {
            resurrected.push(*key);
        }
    }

    let _ = socket.send_to(b"done 0", store);

    // One line with every number, counts first and keys last.
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

    // A finding if any claim broke: an acknowledged write lost, a removal kept, a prepared record
    // lost.
    if lost.is_empty() && resurrected_after_drop.is_empty() && prepared_lost.is_empty() {
        return 0;
    }

    // Aborted, since the store exits later and cleanly; the oracle reads a signal first.
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
        // A missing table holds nothing, as after a crash before the first commit.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(table.get(key)?.is_some())
}
