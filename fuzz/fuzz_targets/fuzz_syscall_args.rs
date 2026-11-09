//! Arbitrary syscalls, with arbitrary arguments, driven into the world.
//!
//! A hostile guest chooses its own registers: addresses that do not map, lengths that do not fit,
//! flags nobody defined, an unsupported number every third call. The world must answer or refuse
//! each one — the supervisor is the single thing in the system that cannot be allowed to fall
//! over, because a supervisor that crashes takes the run's evidence with it.
//!
//! The scripted backend is what makes this a fuzz target rather than an experiment: the guest is
//! a list of operations, so a mutated input is a different program with no process behind it.
#![no_main]

use bohr_backend::GuestSpec;
use bohr_backend_null::{GuestOp, NullBackend, NullProgram};
use bohr_kernel::{run, Oracle, RandomPolicy, RunConfig, RunInputs, RunOutcome, Verdict};
use bohr_tape::Tape;
use bohr_trace::TraceWriter;
use bohr_types::{OracleId, Seed};
use libfuzzer_sys::fuzz_target;

/// Judges nothing: what is under test is that the run reaches an end at all.
struct AcceptEverything;

impl Oracle for AcceptEverything {
    fn identity(&self) -> OracleId {
        OracleId::new("accept-everything", 1)
    }

    fn judge(&mut self, _outcome: &RunOutcome) -> Verdict {
        Verdict::Clean
    }
}

/// Bytes per scripted syscall: a number and six argument registers.
const OP_BYTES: usize = 4 + 6 * 8;

/// Enough operations to reach the interesting states, few enough that a run stays short.
const MAX_OPS: usize = 64;

fuzz_target!(|data: &[u8]| {
    // The first eight bytes choose the tape; the rest are the program. A seed drawn from the
    // input matters because the answers a guest gets are drawn, so the same call can take
    // different paths through the world on different tapes.
    if data.len() < 8 + OP_BYTES {
        return;
    }
    let seed = Seed(u64::from_le_bytes(data[..8].try_into().unwrap_or_default()));
    let body = &data[8..];

    let mut ops = Vec::new();
    for chunk in body.chunks_exact(OP_BYTES).take(MAX_OPS) {
        let nr = u32::from_le_bytes(chunk[..4].try_into().unwrap_or_default());
        let mut args = [0u64; 6];
        for (i, arg) in args.iter_mut().enumerate() {
            let at = 4 + i * 8;
            *arg = u64::from_le_bytes(chunk[at..at + 8].try_into().unwrap_or_default());
        }
        ops.push(GuestOp::Syscall { nr, args });
    }
    if ops.is_empty() {
        return;
    }
    ops.push(GuestOp::Exit(0));

    let tape = Tape::from_seed(seed);
    let mut backend = NullBackend::cluster(vec![NullProgram::single(ops)]);
    let mut world = bohr_world::DistributedWorld::new();
    let mut policy = RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());
    let specs = [GuestSpec::default()];

    // A run that ends in an error is a valid outcome — a refusal is the world working. What is
    // being fuzzed is that it ends: no panic, no unbounded allocation, no loop without a budget.
    let _ = run(RunInputs {
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
    });
});
