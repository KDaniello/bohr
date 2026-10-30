//! A key-value service in front of the store an OP Stack execution client keeps its chain in, and
//! a client that holds it to what it answered.
//!
//! The store is not instrumented and knows nothing about any of this. What is written here is a
//! client, and the only claim it makes is the one every client makes: **a write the store
//! acknowledged is a write the store still has.** A committed transaction is what the
//! acknowledgement rests on, so a record missing after a crash is `commit` not having meant what
//! it says.
//!
//! Two roles in one binary, chosen by the first argument, because both run from the same scenario
//! and a second crate would be a second thing to build.
//!
//! The store is opened exclusively, which is what one process per node actually is. That is not a
//! way around anything: told it is alone with its files, the store stops keeping a reader table for
//! readers that do not exist, and what is left is the path a single node really takes.

use std::{
    error::Error,
    io::ErrorKind,
    net::{SocketAddrV4, UdpSocket},
    path::Path,
    time::Duration,
};

use libmdbx::{DatabaseFlags, Environment, EnvironmentFlags, Geometry, MdbxError, WriteFlags};

/// Whatever the store failed with. The roles only ever format it into an answer, and the store has
/// two error types for the two halves of its API; naming either here would make this file about
/// that split rather than about durability.
type Failed = Box<dyn Error>;

/// Where the store keeps its files, on the node's own filesystem. A directory, because this store
/// wants a data file and a lock file beside each other.
const STORE_PATH: &str = "/store";

/// The one table. A hunt is about durability, not about schema.
const TABLE: &str = "records";

/// Port both roles use. Each node has an address of its own, so one port is enough.
const PORT: u16 = 7000;

/// Bytes stored per record.
const RECORD_BYTES: usize = 256;

/// Largest datagram either role sends.
const MAX_DATAGRAM: usize = 512;

/// How many records the client offers.
const RECORDS_TO_WRITE: u64 = 64;

/// Records a prepared database already holds when the run begins.
const PREPARED_RECORDS: u64 = 2_000;

/// Where a prepared record's key sits, clear of anything the client writes.
const PREPARED_BASE: u64 = 1_000_000;

/// How many prepared records the client checks after the run has had its crashes.
const PREPARED_SAMPLE: u64 = 32;

/// How many polls the client gives one answer before treating it as unanswered.
const POLLS_PER_ANSWER: u32 = 40;

/// How long the client waits between polls, in virtual time.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// How long the store serves before ending.
const STORE_LIFETIME: Duration = Duration::from_secs(6);

/// How large the store may grow.
///
/// Named rather than defaulted, and small. A store left to choose reserves gigabytes of address
/// space it will never touch, which costs a real machine nothing and costs a simulated one a window
/// it has to keep truthful. Sixteen mebibytes is far past what these records need.
const MAP_BYTES: usize = 16 * 1024 * 1024;

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

/// Opens the store, in the one configuration every role here uses.
fn open(at: &Path) -> Result<Environment, MdbxError> {
    Environment::builder()
        .set_max_dbs(2)
        .set_geometry(Geometry { size: Some(0..MAP_BYTES), ..Geometry::default() })
        .set_flags(EnvironmentFlags { exclusive: true, ..EnvironmentFlags::default() })
        .open(at)
}

/// Lays down the database a run starts from, on the host and outside any supervisor.
///
/// Run once, by hand or by a script, and the directory it writes is what `[state] dir` names. It
/// is deliberately not part of a run: a prepared state is an input identified by digest, and a run
/// that built its own would be measuring the building.
fn prepare(dir: &str) -> i32 {
    if dir.is_empty() {
        eprintln!("usage: mdbx-hunt prepare <directory>");
        return 2;
    }
    let root = Path::new(dir).join("store");
    let _ = std::fs::remove_dir_all(&root);
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("cannot make {}: {e}", root.display());
        return 2;
    }

    let environment = match open(&root) {
        Ok(environment) => environment,
        Err(e) => {
            eprintln!("cannot create {}: {e}", root.display());
            return 2;
        }
    };
    // One transaction rather than two thousand: what is wanted is a database with something in it,
    // and the shape of how it got there is the run's business rather than the fixture's.
    let value = vec![b'p'; RECORD_BYTES];
    let write = match environment.begin_rw_sync() {
        Ok(write) => write,
        Err(e) => {
            eprintln!("cannot begin: {e}");
            return 2;
        }
    };
    let table = match write.create_db(Some(TABLE), DatabaseFlags::empty()) {
        Ok(table) => table,
        Err(e) => {
            eprintln!("cannot open the table: {e}");
            return 2;
        }
    };
    for i in 0..PREPARED_RECORDS {
        let key = (PREPARED_BASE + i).to_be_bytes();
        if let Err(e) = write.put(table, key, &value, WriteFlags::empty()) {
            eprintln!("cannot insert: {e}");
            return 2;
        }
    }
    if let Err(e) = write.commit() {
        eprintln!("cannot commit: {e}");
        return 2;
    }
    drop(environment);

    let bytes = std::fs::metadata(root.join("mdbx.dat")).map(|m| m.len()).unwrap_or(0);
    println!("prepared {PREPARED_RECORDS} records in {} ({bytes} bytes)", root.display());
    0
}

/// Writes and reads back without a network, which is the smoke test that the store runs at all.
fn solo() -> i32 {
    let environment = match open(Path::new(STORE_PATH)) {
        Ok(environment) => environment,
        Err(e) => {
            eprintln!("cannot create {STORE_PATH}: {e}");
            return 2;
        }
    };
    for key in 0..RECORDS_TO_WRITE {
        if let Err(e) = commit_record(&environment, key) {
            eprintln!("write {key} failed: {e}");
            return 2;
        }
    }
    for key in 0..RECORDS_TO_WRITE {
        match holds(&environment, key) {
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

/// Answers requests from the database, acknowledging a write only after the commit returns.
///
/// The order is the whole point: the acknowledgement leaves after the commit has returned, so the
/// store has been asked for durability before the client is told it has it.
fn store(index: u64) -> i32 {
    let environment = match open(Path::new(STORE_PATH)) {
        Ok(environment) => environment,
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
            "put" => match commit_record(&environment, key) {
                Ok(()) => format!("ack {key}"),
                Err(e) => format!("err {key} {e}"),
            },
            "get" => match holds(&environment, key) {
                Ok(true) => format!("have {key}"),
                Ok(false) => format!("missing {key}"),
                Err(e) => format!("err {key} {e}"),
            },
            // A removal is a promise like any other: what the store said it took away must stay
            // away, and a crash that brings it back is a resurrection rather than a loss.
            "drop" => match drop_record(&environment, key) {
                Ok(()) => format!("gone {key}"),
                Err(e) => format!("err {key} {e}"),
            },
            "done" => return 0,
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

    // Which prepared records the store actually had before anything happened. Only these are asked
    // for at the end: a scenario that prepared no state has none of them, and holding a store to
    // records nobody ever wrote would report a finding on every run of every empty database.
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
            // what it was told rather than the store breaking a promise.
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
    // decides.
    let mut resurrected = Vec::new();
    for key in &refused {
        if matches!(ask(&socket, store, &format!("get {key}")), Some(a) if a.starts_with("have ")) {
            resurrected.push(*key);
        }
    }

    let _ = socket.send_to(b"done 0", store);

    // One line, always, carrying every number. A run whose client said nothing is a run that
    // checked nothing, and it must not be possible to mistake it for a run that passed.
    // `prepared_seen` is the denominator of the last claim, and without it that claim cannot be
    // read: a run that never found a prepared record reports none lost, which is the same line a
    // run that found two thousand and kept them all prints.
    println!(
        "acknowledged {} verified {} lost {} refused {} resurrected {} undropped {} \
         prepared_seen {} prepared_lost {} · lost={lost:?} resurrected={resurrected:?} \
         undropped={resurrected_after_drop:?} prepared={prepared_lost:?}",
        acknowledged.len(),
        verified,
        lost.len(),
        refused.len(),
        resurrected.len(),
        resurrected_after_drop.len(),
        prepared.len(),
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

/// One record, written and committed.
fn commit_record(environment: &Environment, key: u64) -> Result<(), Failed> {
    let value = vec![b'r'; RECORD_BYTES];
    let write = environment.begin_rw_sync()?;
    let table = write.create_db(Some(TABLE), DatabaseFlags::empty())?;
    write.put(table, key.to_be_bytes(), &value, WriteFlags::empty())?;
    Ok(write.commit()?)
}

/// One record, taken away and committed.
fn drop_record(environment: &Environment, key: u64) -> Result<(), Failed> {
    let write = environment.begin_rw_sync()?;
    let table = write.create_db(Some(TABLE), DatabaseFlags::empty())?;
    write.del(table, &key.to_be_bytes(), None)?;
    Ok(write.commit()?)
}

/// Whether the store still holds a record.
///
/// A read that fails is reported as a failure and never as an absence: telling "the store lost it"
/// apart from "the store could not be asked" is the whole difference between a finding and a
/// question nobody answered.
fn holds(environment: &Environment, key: u64) -> Result<bool, Failed> {
    let read = environment.begin_ro_sync()?;
    // A table that does not exist holds nothing, which is a legitimate answer to the question and
    // not an error: a crash before the first commit leaves exactly this.
    let Ok(table) = read.open_db(Some(TABLE)) else {
        return Ok(false);
    };
    Ok(read.get::<Vec<u8>>(table.dbi(), &key.to_be_bytes())?.is_some())
}
