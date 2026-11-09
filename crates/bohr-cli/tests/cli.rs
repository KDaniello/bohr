//! The command line as a contract: what it prints, what it writes and what it returns.
//!
//! Exit codes are checked by running the binary rather than the library, because a pipeline
//! branches on the process result and nothing else.

use std::process::Command;

use bohr_trace::{Bundle, Outcome, SectionId, read_events};
use bohr_types::{OracleId, WorldModelId};

fn bohr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bohr"))
}

/// Standard output exactly as it was written, since what a guest prints is not text.
fn guest_output(args: &[&str]) -> Vec<u8> {
    bohr().args(args).output().expect("the binary must be runnable").stdout
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = bohr().args(args).output().expect("the binary must be runnable");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn version_reports_every_contract_number() {
    let (code, stdout, _) = run(&["version", "--verbose"]);
    assert_eq!(code, 0);
    for expected in ["tape ABI", "bundle format", "SDK schema"] {
        assert!(stdout.contains(expected), "`version --verbose` omitted {expected}: {stdout}");
    }
}

#[test]
fn a_reproducing_selftest_exits_clean() {
    let (code, _, stderr) = run(&["selftest", "--repeat", "20", "--seed", "7"]);
    assert_eq!(code, 0, "what it said was: {stderr}");
    assert!(stderr.contains("clock-and-random"));
    assert!(stderr.contains("two-tasks"));
    assert!(!stderr.contains("DIVERGED"));
}

#[test]
fn the_same_seed_reports_the_same_digest_across_invocations() {
    let (_, _, a) = run(&["selftest", "--repeat", "2", "--seed", "5", "--guest", "two-tasks"]);
    let (_, _, b) = run(&["selftest", "--repeat", "2", "--seed", "5", "--guest", "two-tasks"]);
    assert_eq!(a, b, "two invocations of one seed must print the same digest");

    let (_, _, c) = run(&["selftest", "--repeat", "2", "--seed", "6", "--guest", "two-tasks"]);
    assert_ne!(a, c, "a different seed must reach a different digest");
}

/// What a guest writes and what the tool says about the run travel on different streams, so a
/// pipeline receives the guest's bytes and nothing else.
#[test]
fn the_report_stays_clear_of_the_guests_own_output() {
    let args = ["selftest", "--repeat", "2", "--seed", "3", "--guest", "clock-and-random"];
    let (code, _, stderr) = run(&args);
    assert_eq!(code, 0, "{stderr}");

    // The guest writes thirty-two bytes and nothing else; the report carries its name and never
    // reaches the stream those bytes travel on.
    let bytes = guest_output(&args);
    assert_eq!(bytes.len(), 32, "standard output belongs to the guest");
    assert!(stderr.contains("clock-and-random"), "the report must reach standard error");

    assert_eq!(bytes, guest_output(&args), "one seed, one set of bytes on the pipe");
}

#[test]
fn a_bad_invocation_is_a_usage_error_not_a_finding() {
    // Exit 2 rather than 1: a pipeline must be able to tell "the tool found a bug" from
    // "the tool was called wrongly".
    let (code, _, stderr) = run(&["selftest", "--repeat", "0"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("BOHR-1002"), "the error must carry its code: {stderr}");

    let (code, _, stderr) = run(&["run", "--", "./nothing-here"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("BOHR-1001"), "the error must carry its code: {stderr}");
    assert!(stderr.contains("./nothing-here"), "the error must name what was asked for");
}

#[test]
fn a_saved_bundle_is_self_describing_and_refuses_foreign_components() {
    let dir = std::env::temp_dir().join("bohr-cli-bundle-test");
    let _ = std::fs::remove_dir_all(&dir);
    let dir_str = dir.to_string_lossy().into_owned();

    let (code, _, stderr) =
        run(&["selftest", "--repeat", "3", "--guest", "clock-and-random", "--save-dir", &dir_str]);
    assert_eq!(code, 0, "{stderr}");

    let bytes = std::fs::read(dir.join("clock-and-random.bohr")).expect("bundle written");
    let bundle = Bundle::parse(&bytes).expect("a written bundle must parse");
    let manifest = bundle.manifest().expect("manifest");

    assert_eq!(manifest.executions, 1);
    assert_eq!(manifest.outcome, Outcome::Clean);
    assert_eq!(manifest.determinism.level, "full");
    assert!(manifest.trace_digest.starts_with("blake3:"));

    // Every section a replay would need is present and readable.
    let events = read_events(bundle.section(SectionId::Events).expect("events"))
        .expect("the stored log must parse");
    assert!(!events.is_empty());
    assert!(bundle.section(SectionId::Tape).is_some());
    assert!(bundle.section(SectionId::DrawJournal).is_some());

    assert!(
        manifest
            .check_provenance(
                &WorldModelId::new("minimal", 2),
                &OracleId::new("properties+crash", 1)
            )
            .is_ok()
    );
    assert!(
        manifest
            .check_provenance(
                &WorldModelId::new("distributed", 1),
                &OracleId::new("properties+crash", 1)
            )
            .is_err(),
        "a bundle must refuse a world model that did not produce it"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An option the build cannot honour is refused by name. Accepting it and running something else
/// would report a run under a policy that never executed.
#[test]
fn an_option_this_build_cannot_honour_is_refused() {
    let (code, _, stderr) = run(&["run", "--scheduler", "adversarial", "--", "./whatever"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("scheduler"), "the refusal must name what was refused: {stderr}");
    assert!(stderr.contains("pct"), "and what is available instead: {stderr}");
}

/// A stored tape and a seed are two answers to the same question, so naming both is refused rather
/// than resolved by precedence.
#[test]
fn a_seed_and_a_tape_together_are_refused() {
    let (code, _, stderr) = run(&["run", "--seed", "1", "--tape", "t.tape", "--", "./whatever"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("BOHR-1004"), "the refusal must carry its code: {stderr}");
}

/// A null path argument is answered, not treated as memory the supervisor failed to read.
///
/// The guest asserts the answer itself, so a wrong `errno` is exit 2 rather than a passing run:
/// what matters is that the number reaching the guest is the one Linux gives, since `glibc` reads
/// it as a statement about the kernel and branches its whole startup on it.
#[test]
fn a_null_path_is_answered_rather_than_ending_the_run() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/guests/build/null_path");
    if !path.is_file() {
        return;
    }
    let (code, _, stderr) = run(&["run", "--seed", "1", "--", &path.to_string_lossy()]);
    assert_eq!(code, 0, "the guest judged its own answers and said: {stderr}");
}

/// Symbolic links, duplicated descriptors, the oldest spelling of the clock, and taking a
/// directory away.
///
/// The guest judges every answer itself and returns a different number for each promise, so a
/// model that invented one of them is a named failure rather than a run that passed. Measured
/// against a real target: none of these were modelled, and `solana-test-validator` met them in
/// this order before it reached its own storage.
#[test]
fn names_and_descriptors_answer_the_way_the_kernel_does() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/guests/build/links_and_descriptors");
    if !path.is_file() {
        return;
    }
    let (code, _, stderr) = run(&["run", "--seed", "1", "--", &path.to_string_lossy()]);
    assert_eq!(code, 0, "the guest judged its own answers and said: {stderr}");
}

/// A process that ends while its other threads are alive is an ordinary run, not a broken tool.
///
/// The world declares the node gone on the call that ended it and takes the rest of its tasks off
/// the books there; the backend then reports each thread's own exit. Reading those as exits for
/// tasks nobody created ended the run with an internal error — on the shape every threaded
/// program stops in.
#[test]
fn a_process_that_ends_with_threads_running_is_not_an_internal_error() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/guests/build/exits_with_threads_running");
    if !path.is_file() {
        return;
    }
    let (code, _, stderr) = run(&["run", "--seed", "1", "--", &path.to_string_lossy()]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("BOHR-5001"), "no invariant may fail on this shape: {stderr}");
}

/// A name is taken from the directory the call named, not from wherever the program stands.
///
/// The guest judges every answer and returns a different number for each, and one of its checks is
/// that the file is *not* in the working directory. An `*at` call whose directory descriptor
/// nothing read would put every relative name there and report success -- worse than a refusal,
/// because what the guest believed it had written and what the world held would part with nothing
/// reporting anything wrong.
#[test]
fn a_relative_name_is_taken_from_the_directory_that_was_named() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/guests/build/paths_under_a_directory");
    if !path.is_file() {
        return;
    }
    let (code, _, stderr) = run(&["run", "--seed", "1", "--", &path.to_string_lossy()]);
    assert_eq!(code, 0, "the guest judged its own answers and said: {stderr}");
}

/// The processor question is answered from this world's own profile, on a host that cannot trap it.
///
/// The instruction is rewritten in a copy of the program, and the fault it raises is recognised by
/// the byte at the fault address rather than by a table of offsets — which is what lets it work on
/// the program itself, mapped by the kernel where nothing on this side knows where it landed.
///
/// The run must also stop calling itself degraded for a facility it no longer needs: a run whose
/// guest asks and is answered is determined, whatever the host could not have done.
#[test]
fn a_processor_question_is_answered_where_the_host_cannot_trap_it() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/guests/build/clock_and_random");
    if !path.is_file() || !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return;
    }
    let program = path.to_string_lossy().into_owned();

    let (code, _, stderr) = run(&["run", "--seed", "1", "--patch-cpuid", "--", &program]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("processor question"), "the rewrite must say what it did:\n{stderr}");
    assert!(
        !stderr.contains("cpuid_not_trappable"),
        "a run whose questions are answered is not degraded for the host:\n{stderr}"
    );

    let (_, _, plain) = run(&["run", "--seed", "1", "--", &program]);
    assert!(
        plain.contains("cpuid_not_trappable") || plain.contains("cpuid_not_trapped"),
        "and without the rewrite it still is, or this test proves nothing:\n{plain}"
    );
}
