//! End-to-end runs through the whole stack above interception.
//!
//! Tape, clock, scheduler, world model, event log and digest with a scripted guest, on any machine
//! in milliseconds.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullBackend, NullProgram};
use bohr_kernel::{Oracle, RunConfig, RunInputs, RunOutcome, Verdict, run};
use bohr_tape::Tape;
use bohr_trace::{EventKind, Outcome, TraceWriter, read_events};
use bohr_types::{Digest, OracleId, Seed};
use bohr_world::MinimalWorld;

/// Reports a non-zero exit as a crash and has no opinion otherwise.
struct ExitStatusOracle;

impl Oracle for ExitStatusOracle {
    fn identity(&self) -> OracleId {
        OracleId::new("exit-status", 1)
    }

    fn judge(&mut self, run: &RunOutcome) -> Verdict {
        if let Some(signal) = run.killed_by {
            return Verdict::Finding(Outcome::GuestCrashed(format!("killed by signal {signal}")));
        }
        match run.exit {
            Some(0) | None => Verdict::Clean,
            Some(code) => Verdict::Finding(Outcome::GuestCrashed(format!("exit status {code}"))),
        }
    }
}

const CLOCK_GETTIME: u32 = 228;
const GETRANDOM: u32 = 318;
const WRITE: u32 = 1;

/// Reads the clock, asks for randomness and writes it out.
fn clock_and_random() -> NullProgram {
    NullProgram::single(vec![
        GuestOp::Syscall { nr: CLOCK_GETTIME, args: [0, MEMORY_BASE, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: GETRANDOM, args: [MEMORY_BASE + 64, 16, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: WRITE, args: [1, MEMORY_BASE + 64, 16, 0, 0, 0] },
        GuestOp::Syscall { nr: CLOCK_GETTIME, args: [0, MEMORY_BASE + 32, 0, 0, 0, 0] },
        GuestOp::Exit(0),
    ])
}

#[derive(Debug)]
struct Completed {
    digest: Digest,
    events: u64,
    log: Vec<u8>,
    guest_random: Vec<u8>,
    outcome: Outcome,
    draws: usize,
    /// Where on the tape the guest's randomness came from.
    random_draw_offset: Option<u64>,
}

fn execute(program: NullProgram, seed: u64) -> Result<Completed, String> {
    execute_with_memory(program, seed, &[])
}

fn execute_with_memory(
    program: NullProgram,
    seed: u64,
    memory: &[(u64, Vec<u8>)],
) -> Result<Completed, String> {
    let tape = Tape::from_seed(Seed(seed));
    let mut backend = NullBackend::new(program);
    for (addr, bytes) in memory {
        backend.preload(bohr_types::GuestAddr(*addr), bytes).map_err(|e| e.to_string())?;
    }
    let mut world = MinimalWorld::new();
    let mut policy = bohr_kernel::RandomPolicy;
    let mut oracle = ExitStatusOracle;
    let mut sink = TraceWriter::new(Vec::new());
    let spec = bohr_backend::GuestSpec::default();

    let report = run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape: &tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: std::slice::from_ref(&spec),
        config: RunConfig::default(),
    })
    .map_err(|e| e.to_string())?;

    let guest_random = backend.memory()[64..80].to_vec();
    let (log, digest, _) = sink.finish().map_err(|e| e.to_string())?;
    assert_eq!(digest, report.digest, "the sink and the report must agree on what ran");

    let random_draw_offset = report
        .journal
        .records()
        .iter()
        .find(|r| r.site == bohr_tape::DrawSite::GuestRandom as u16)
        .map(|r| r.offset);

    Ok(Completed {
        digest: report.digest,
        events: report.events,
        log,
        guest_random,
        outcome: report.outcome,
        draws: report.journal.len(),
        random_draw_offset,
    })
}

fn execute_cluster(programs: Vec<NullProgram>, seed: u64) -> Result<(Digest, Vec<u8>), String> {
    let tape = Tape::from_seed(Seed(seed));
    let mut backend = NullBackend::cluster(programs.clone());
    let mut world = MinimalWorld::new();
    let mut policy = bohr_kernel::RandomPolicy;
    let mut oracle = ExitStatusOracle;
    let mut sink = TraceWriter::new(Vec::new());
    let specs = vec![bohr_backend::GuestSpec::default(); programs.len()];

    let report = run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape: &tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: &specs,
        config: RunConfig::default(),
    })
    .map_err(|e| e.to_string())?;
    let (log, digest, _) = sink.finish().map_err(|e| e.to_string())?;
    assert_eq!(digest, report.digest);
    Ok((digest, log))
}

#[test]
fn two_nodes_run_reproducibly_with_clocks_of_their_own() {
    let node = || {
        NullProgram::single(vec![
            GuestOp::Syscall { nr: CLOCK_GETTIME, args: [0, MEMORY_BASE, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: GETRANDOM, args: [MEMORY_BASE + 64, 8, 0, 0, 0, 0] },
            GuestOp::Exit(0),
        ])
    };

    let (first, log) = execute_cluster(vec![node(), node()], 21).expect("run");
    for attempt in 0..200 {
        let (again, log_again) = execute_cluster(vec![node(), node()], 21).expect("run");
        assert_eq!(first, again, "attempt {attempt} diverged");
        assert_eq!(log, log_again, "the recorded log is not byte-identical");
    }

    // Each node has its own skew, so the first readings differ.
    let events = read_events(&log).expect("read log");
    let mut first_read = std::collections::BTreeMap::new();
    for event in &events {
        if let EventKind::ClockRead { presented } = event.kind {
            first_read.entry(event.node).or_insert(presented);
        }
    }
    assert_eq!(first_read.len(), 2, "both nodes read the clock");
    let reads: Vec<_> = first_read.values().collect();
    assert_ne!(reads[0], reads[1], "two nodes never agree on the time");
}

#[test]
fn one_seed_produces_one_run() {
    let first = execute(clock_and_random(), 42).expect("run");
    for attempt in 0..200 {
        let again = execute(clock_and_random(), 42).expect("run");
        assert_eq!(first.digest, again.digest, "attempt {attempt} diverged");
        assert_eq!(first.events, again.events);
        assert_eq!(first.guest_random, again.guest_random, "the guest saw different randomness");
        assert_eq!(first.log, again.log, "the recorded log is not byte-identical");
    }
}

#[test]
fn a_run_with_one_task_records_no_switches() {
    let done = execute(clock_and_random(), 42).expect("run");
    let events = read_events(&done.log).expect("read log");
    assert!(
        events.iter().all(|e| !matches!(e.kind, EventKind::Schedule { .. })),
        "execution never moved between tasks, yet a switch was recorded"
    );
}

#[test]
fn a_different_seed_produces_a_different_run() {
    let a = execute(clock_and_random(), 1).expect("run");
    let b = execute(clock_and_random(), 2).expect("run");
    assert_ne!(a.guest_random, b.guest_random, "the guest saw the same bytes from two seeds");
    assert_ne!(a.digest, b.digest);
}

#[test]
fn the_guest_receives_tape_bytes_rather_than_host_entropy() {
    let completed = execute(clock_and_random(), 7).expect("run");
    let tape = Tape::from_seed(Seed(7));

    // Read where the journal says, since the clock draws come first.
    let at = completed
        .random_draw_offset
        .expect("the run must have drawn the randomness it handed the guest");
    let mut expected = [0u8; 16];
    tape.read_at(at, &mut expected);
    assert_eq!(completed.guest_random, expected, "randomness did not come from the tape");
    assert!(completed.draws >= 1);
}

#[test]
fn virtual_time_advances_and_is_what_the_guest_is_told() {
    let completed = execute(clock_and_random(), 3).expect("run");
    let events = read_events(&completed.log).expect("read log");

    let clock_reads: Vec<u64> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ClockRead { presented } => Some(presented.get()),
            _ => None,
        })
        .collect();

    assert_eq!(clock_reads.len(), 2, "the guest read the clock twice");
    assert!(clock_reads[1] > clock_reads[0], "time must move between two reads");
    assert_eq!(completed.outcome, Outcome::Clean);
}

#[test]
fn an_unsupported_syscall_is_refused_by_name() {
    // 41 is `socket`, which this model refuses.
    let program =
        NullProgram::single(vec![GuestOp::Syscall { nr: 41, args: [0; 6] }, GuestOp::Exit(0)]);
    let err = execute(program, 1).expect_err("an unmodelled syscall must not be answered");
    assert!(err.contains("unsupported syscall 41"), "the error must name the syscall: {err}");
}

#[test]
fn a_non_zero_exit_is_a_finding() {
    let program = NullProgram::single(vec![GuestOp::Exit(3)]);
    let completed = execute(program, 1).expect("run");
    assert!(
        matches!(completed.outcome, Outcome::GuestCrashed(ref m) if m.contains('3')),
        "got {:?}",
        completed.outcome
    );
}

#[test]
fn interleaving_two_tasks_stays_reproducible() {
    let program = || NullProgram {
        main: vec![
            GuestOp::Spawn,
            GuestOp::Syscall { nr: GETRANDOM, args: [MEMORY_BASE, 8, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: CLOCK_GETTIME, args: [0, MEMORY_BASE + 16, 0, 0, 0, 0] },
            GuestOp::Exit(0),
        ],
        children: vec![vec![
            GuestOp::Syscall { nr: GETRANDOM, args: [MEMORY_BASE + 32, 8, 0, 0, 0, 0] },
            GuestOp::Syscall { nr: CLOCK_GETTIME, args: [0, MEMORY_BASE + 48, 0, 0, 0, 0] },
            GuestOp::Exit(0),
        ]],
    };

    let first = execute(program(), 11).expect("run");
    for _ in 0..100 {
        assert_eq!(execute(program(), 11).expect("run").digest, first.digest);
    }

    // With two runnable tasks the seed matters.
    let other = execute(program(), 12).expect("run");
    assert_ne!(other.digest, first.digest, "the schedule did not depend on the tape");

    let events = read_events(&first.log).expect("read log");
    let switches = events.iter().filter(|e| matches!(e.kind, EventKind::Schedule { .. })).count();
    assert!(switches >= 2, "two tasks should produce at least two scheduling decisions");
}

#[test]
fn a_guest_that_never_ends_is_stopped_by_the_tick_budget() {
    let mut ops = Vec::new();
    for _ in 0..100_000 {
        ops.push(GuestOp::Syscall { nr: CLOCK_GETTIME, args: [0, MEMORY_BASE, 0, 0, 0, 0] });
    }
    let tape = Tape::from_seed(Seed(1));
    let mut backend = NullBackend::new(NullProgram::single(ops));
    let mut world = MinimalWorld::new();
    let mut policy = bohr_kernel::RandomPolicy;
    let mut oracle = ExitStatusOracle;
    let mut sink = TraceWriter::new(Vec::new());
    let spec = bohr_backend::GuestSpec::default();

    let report = run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape: &tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: std::slice::from_ref(&spec),
        // One millisecond of virtual time buys a thousand syscalls at the model's cost.
        config: RunConfig { max_ticks: bohr_types::Tick::MILLISECOND, ..RunConfig::default() },
    })
    .expect("run");

    assert!(matches!(report.outcome, Outcome::GuestHung(_)), "got {:?}", report.outcome);
    assert!(report.ticks >= bohr_types::Tick::MILLISECOND);
}

/// A fresh descriptor is the lowest free number, as the kernel allocates.
#[test]
fn descriptors_are_the_lowest_free_number() {
    const OPENAT: u32 = 257;
    const CLOSE: u32 = 3;
    const PATH_AT: u64 = MEMORY_BASE + 2048;

    let mut path = b"/dev/urandom\0".to_vec();
    path.resize(16, 0);

    let program = NullProgram::single(vec![
        GuestOp::Syscall { nr: OPENAT, args: [0, PATH_AT, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: OPENAT, args: [0, PATH_AT, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: CLOSE, args: [3, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: OPENAT, args: [0, PATH_AT, 0, 0, 0, 0] },
        GuestOp::Exit(0),
    ]);

    let completed = execute_with_memory(program, 4, &[(PATH_AT, path)]).expect("run");
    let returns: Vec<i64> = read_events(&completed.log)
        .expect("log")
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Syscall { nr, ret, .. } if nr == OPENAT || nr == CLOSE => Some(ret),
            _ => None,
        })
        .collect();
    assert_eq!(returns, vec![3, 4, 0, 3], "a closed descriptor is the next one handed out");
}
