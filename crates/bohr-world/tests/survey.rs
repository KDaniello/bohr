//! A survey collects what a program needs and this build does not have.
//!
//! A run stops at the first unmodelled call (§5.7); a survey collects them all. These tests also
//! keep a survey from passing as a run.

use bohr_backend::GuestSpec;
use bohr_backend_null::{GuestOp, NullBackend, NullProgram};
use bohr_kernel::{
    KernelError, Oracle, RandomPolicy, RunConfig, RunInputs, RunOutcome, Verdict, WorldModel, run,
};
use bohr_tape::Tape;
use bohr_trace::{EventKind, TraceWriter, read_events};
use bohr_types::{OracleId, Seed};
use bohr_world::MinimalWorld;

/// Judges nothing: what is under test is what the world did.
struct AcceptEverything;

impl Oracle for AcceptEverything {
    fn identity(&self) -> OracleId {
        OracleId::new("accept-everything", 1)
    }

    fn judge(&mut self, _outcome: &RunOutcome) -> Verdict {
        Verdict::Clean
    }
}

/// `io_uring_enter`, which no build here models: opening a ring is answered, entering one is not.
const IO_URING_ENTER: u32 = 426;
/// `epoll_create1`, an ordinary unmodelled call.
const EPOLL_CREATE1: u32 = 291;
/// `getpid`, which every build models.
const GETPID: u32 = 39;
/// `inotify_init1`, answered `ENOSYS` on purpose so callers poll.
const INOTIFY_INIT1: u32 = 294;

fn call(nr: u32) -> GuestOp {
    GuestOp::Syscall { nr, args: [0; 6] }
}

/// Runs a scripted guest through the world it is handed, and returns the diagnostics recorded.
fn drive(
    world: &mut MinimalWorld,
    ops: Vec<GuestOp>,
) -> Result<Vec<(String, String)>, KernelError> {
    let tape = Tape::from_seed(Seed(1));
    let mut backend = NullBackend::new(NullProgram::single(ops));
    let mut policy = RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());

    run(RunInputs {
        arming: bohr_kernel::Arming::NONE,
        tape: &tape,
        backend: &mut backend,
        world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: None,
        specs: std::slice::from_ref(&GuestSpec::default()),
        config: RunConfig::default(),
    })?;

    let (log, _, _) = sink.finish().expect("the sink closes");
    Ok(read_events(&log)
        .expect("the log decodes")
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Diagnostic { code, message } => Some((code, message)),
            _ => None,
        })
        .collect())
}

/// A survey reaches the second unmodelled call.
#[test]
fn a_survey_gets_past_the_first_call_it_cannot_model() {
    let mut world = MinimalWorld::new().surveying();
    let diagnostics =
        drive(&mut world, vec![call(IO_URING_ENTER), call(EPOLL_CREATE1), call(IO_URING_ENTER)])
            .expect("a survey answers rather than refusing");

    assert_eq!(
        world.surveyed(),
        vec![(EPOLL_CREATE1, 1), (IO_URING_ENTER, 2)],
        "every unmodelled call is counted, and counted once per time it was asked for"
    );
    assert_eq!(
        diagnostics.len(),
        2,
        "one diagnostic per distinct number, not per call: {diagnostics:?}"
    );
}

/// A survey says what it faked, on the timeline, under the same code the refusal would have used.
///
/// The log says so too.
#[test]
fn every_faked_answer_is_named_where_the_run_is_read() {
    let mut world = MinimalWorld::new().surveying();
    let diagnostics = drive(&mut world, vec![call(IO_URING_ENTER), call(EPOLL_CREATE1)])
        .expect("a survey answers rather than refusing");

    let codes: Vec<&str> = diagnostics.iter().map(|(code, _)| code.as_str()).collect();
    assert!(codes.contains(&"BOHR-2004"), "the ring keeps its own code: {codes:?}");
    assert!(codes.contains(&"BOHR-2001"), "everything else keeps the general one: {codes:?}");
    for (_, message) in &diagnostics {
        assert!(message.contains("ENOSYS"), "a faked answer must say so: {message}");
    }
}

/// A survey world has its own identity, which `Manifest::check_provenance` refuses.
#[test]
fn a_survey_is_a_different_world_and_says_so() {
    let ordinary = MinimalWorld::new();
    let surveying = MinimalWorld::new().surveying();

    assert_eq!(ordinary.identity().name, "minimal");
    assert_eq!(surveying.identity().name, "minimal-survey");
    assert_ne!(ordinary.identity(), surveying.identity());
}

/// Without the mode, nothing changes: the first unmodelled call still ends the run by name.
#[test]
fn an_ordinary_world_still_refuses_the_first_one() {
    let mut world = MinimalWorld::new();
    let refused = drive(&mut world, vec![call(EPOLL_CREATE1)]).expect_err("§5.7 refuses this");

    assert!(format!("{refused}").contains("BOHR-2001"), "{refused}");
    assert!(world.surveyed().is_empty(), "a world that is not surveying records nothing");
}

/// Modelled calls are answered by the model, surveying or not.
#[test]
fn a_survey_only_answers_what_nothing_else_would() {
    let mut world = MinimalWorld::new().surveying();
    let diagnostics = drive(&mut world, vec![call(GETPID)]).expect("a modelled call is modelled");

    assert!(world.surveyed().is_empty(), "getpid is modelled and must not be recorded");
    assert!(diagnostics.is_empty(), "nothing was faked, so nothing is reported: {diagnostics:?}");
}

/// A wall is a wall: a call answered `ENOSYS` on purpose is collected like one nothing models.
///
/// The report says which kind each is.
#[test]
fn a_call_answered_on_purpose_is_still_a_wall() {
    let mut world = MinimalWorld::new().surveying();
    let diagnostics = drive(&mut world, vec![call(INOTIFY_INIT1), call(INOTIFY_INIT1)])
        .expect("the answer is given rather than the run refused");

    assert_eq!(
        world.surveyed(),
        vec![(INOTIFY_INIT1, 2)],
        "a deliberate ENOSYS is counted like any other wall"
    );
    assert_eq!(diagnostics.len(), 1, "once per number, not per call: {diagnostics:?}");

    // An ordinary run answers silently.
    let mut ordinary = MinimalWorld::new();
    let quiet =
        drive(&mut ordinary, vec![call(INOTIFY_INIT1)]).expect("still answered, and still quietly");
    assert!(ordinary.surveyed().is_empty(), "a world that is not surveying records nothing");
    assert!(quiet.is_empty(), "and says nothing on the timeline: {quiet:?}");
}
