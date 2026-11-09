//! The scenarios in `tests/features`, run against the built binary.
//!
//! These are the contracts quoted outside the team — in documentation, in an issue, in an answer
//! to "what does this tool actually promise". They are written in the product's own vocabulary and
//! they drive the command line, because a promise checked against an internal function is a
//! promise about the wrong thing. See `docs/TESTING.md` §5.

use std::{path::PathBuf, process::Command};

use cucumber::{World as _, given, then, when};

/// Whether this build can intercept a program on disk.
const HAS_BACKEND: bool = cfg!(all(target_os = "linux", target_arch = "x86_64"));

/// What one invocation left behind.
#[derive(Debug, Default, Clone)]
struct Invocation {
    /// The exit code, or -1 where the command could not be run at all.
    code: i32,
    /// Everything the command said, both streams joined in the order a user sees them.
    output: String,
}

impl Invocation {
    /// The digest a run reports, which is the last field of its report line.
    fn digest(&self) -> Option<String> {
        self.output
            .lines()
            .find(|line| line.contains(" runs · "))
            .and_then(|line| line.rsplit('·').next())
            .map(|d| d.trim().to_owned())
    }
}

/// What a scenario has done so far.
#[derive(Debug, Default, cucumber::World)]
struct Bohr {
    /// The built-in guest under test, where one was named.
    guest: String,
    /// The program under test, where one was named.
    program: String,
    /// Where bundles and scratch files of this scenario live.
    dir: Option<PathBuf>,
    /// The bundle a scenario saved, altered or replayed.
    bundle: PathBuf,
    /// The invocations so far, in order.
    runs: Vec<Invocation>,
}

impl Bohr {
    /// Runs the tool and remembers what it said.
    fn bohr(&mut self, args: &[&str]) {
        let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
            .args(args)
            .output()
            .expect("the binary under test must be runnable");
        let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
        output.push_str(&String::from_utf8_lossy(&out.stderr));
        self.runs.push(Invocation { code: out.status.code().unwrap_or(-1), output });
    }

    /// A directory of this scenario's own, named after what the scenario is about so that a
    /// failure leaves something a person can go and look at.
    fn dir(&mut self) -> PathBuf {
        let guest = self.guest.clone();
        self.dir
            .get_or_insert_with(|| {
                let dir = std::env::temp_dir().join(format!("bohr-bdd-{guest}"));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).expect("a directory of this scenario's own");
                dir
            })
            .clone()
    }

    /// The most recent invocation.
    fn last(&self) -> &Invocation {
        self.runs.last().expect("a step must have run the tool first")
    }
}

#[given(expr = "the built-in guest {string}")]
fn given_guest(world: &mut Bohr, guest: String) {
    world.guest = guest;
}

#[given(expr = "the program {string}")]
fn given_program(world: &mut Bohr, program: String) {
    world.program = repo_root().join(program).to_string_lossy().into_owned();
}

#[given("a file that is not a bundle")]
fn given_not_a_bundle(world: &mut Bohr) {
    let path = world.dir().join("not-a-bundle.bohr");
    std::fs::write(&path, b"this is not a bundle, and says so").expect("the file can be written");
    world.bundle = path;
}

#[when(expr = "I run it {int} times with seed {int}")]
fn when_run_repeatedly(world: &mut Bohr, times: usize, seed: u64) {
    let guest = world.guest.clone();
    world.bohr(&[
        "selftest",
        "--guest",
        &guest,
        "--seed",
        &seed.to_string(),
        "--repeat",
        &times.to_string(),
    ]);
}

#[when(expr = "I run it once with seed {int}")]
fn when_run_once(world: &mut Bohr, seed: u64) {
    when_run_repeatedly(world, 1, seed);
}

#[when(expr = "I run it again with seed {int}")]
fn when_run_again(world: &mut Bohr, seed: u64) {
    when_run_repeatedly(world, 1, seed);
}

#[when(expr = "I run the program once with seed {int}")]
fn when_run_program(world: &mut Bohr, seed: u64) {
    if !HAS_BACKEND {
        return;
    }
    let program = world.program.clone();
    world.bohr(&["run", "--seed", &seed.to_string(), "--scheduler", "random", "--", &program]);
}

#[when(expr = "I run it once with seed {int} and keep the bundle")]
fn when_run_and_keep(world: &mut Bohr, seed: u64) {
    let guest = world.guest.clone();
    let dir = world.dir();
    world.bohr(&[
        "selftest",
        "--guest",
        &guest,
        "--seed",
        &seed.to_string(),
        "--repeat",
        "1",
        "--save-dir",
        &dir.to_string_lossy(),
    ]);
    world.bundle = dir.join(format!("{guest}.bohr"));
}

#[when("I alter a byte in the middle of the bundle")]
fn when_alter(world: &mut Bohr) {
    let mut bytes = std::fs::read(&world.bundle).expect("the bundle was saved");
    let at = bytes.len() / 2;
    bytes[at] ^= 0xff;
    std::fs::write(&world.bundle, bytes).expect("the bundle can be rewritten");
}

#[when("I replay the bundle")]
fn when_replay(world: &mut Bohr) {
    let bundle = world.bundle.to_string_lossy().into_owned();
    world.bohr(&["replay", &bundle]);
}

#[when("I ask this host what it can do")]
fn when_doctor(world: &mut Bohr) {
    world.bohr(&["doctor"]);
}

#[when("I ask for the syscall table")]
fn when_syscalls(world: &mut Bohr) {
    world.bohr(&["doctor", "--syscalls"]);
}

#[then("the run succeeds")]
fn then_run_succeeds(world: &mut Bohr) {
    let run = world.last();
    assert_eq!(run.code, 0, "the run did not succeed:\n{}", run.output);
}

#[then("the replay succeeds")]
fn then_replay_succeeds(world: &mut Bohr) {
    then_run_succeeds(world);
}

#[then("the replay fails")]
fn then_replay_fails(world: &mut Bohr) {
    let run = world.last();
    assert_ne!(run.code, 0, "a bundle that cannot be trusted was accepted:\n{}", run.output);
}

#[then(expr = "the exit code is {int}")]
fn then_exit_code(world: &mut Bohr, code: i32) {
    if world.runs.is_empty() {
        return;
    }
    let run = world.last();
    assert_eq!(run.code, code, "the wrong code was returned:\n{}", run.output);
}

#[then("the report says every run agreed")]
fn then_every_run_agreed(world: &mut Bohr) {
    let run = world.last();
    assert!(!run.output.contains("DIVERGED"), "the runs did not agree:\n{}", run.output);
    assert!(run.output.contains(" runs · "), "nothing was reported:\n{}", run.output);
}

#[then("the two runs report different digests")]
fn then_digests_differ(world: &mut Bohr) {
    let mut seen = world.runs.iter().filter_map(Invocation::digest);
    let first = seen.next().expect("the first run reported a digest");
    let second = seen.next().expect("the second run reported a digest");
    assert_ne!(first, second, "two seeds produced the same run");
}

#[then("the replay reports the recorded digest")]
fn then_replay_reports_digest(world: &mut Bohr) {
    let run = world.last();
    assert!(
        run.output.contains(": ok "),
        "the replay said nothing about the digest:\n{}",
        run.output
    );
}

#[then("the message names an error code")]
fn then_names_a_code(world: &mut Bohr) {
    let run = world.last();
    assert!(run.output.contains("BOHR-"), "the refusal named no code:\n{}", run.output);
}

/// Named rather than merely present: a refusal that carried the wrong code would satisfy "names an
/// error code" while sending a reader to the wrong page.
#[then(expr = "the message names error code {string}")]
fn then_names_this_code(world: &mut Bohr, code: String) {
    if !HAS_BACKEND {
        return;
    }
    let run = world.last();
    assert!(run.output.contains(&code), "the refusal did not name {code}:\n{}", run.output);
}

#[then("the report names the finding")]
fn then_names_the_finding(world: &mut Bohr) {
    if !HAS_BACKEND {
        return;
    }
    let run = world.last();
    assert!(run.output.contains("finding"), "the finding was not reported:\n{}", run.output);
}

#[then("a verdict is reached")]
fn then_verdict(world: &mut Bohr) {
    let run = world.last();
    assert!(
        run.output.contains("this host can") || run.output.contains("cannot run guests"),
        "no verdict was reached:\n{}",
        run.output
    );
}

#[then("every failure carries something to type")]
fn then_failures_carry_a_remedy(world: &mut Bohr) {
    let run = world.last();
    let lines: Vec<&str> = run.output.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.contains("FAIL") {
            let next = lines.get(i + 1).copied().unwrap_or_default();
            assert!(
                next.starts_with(' ') && !next.trim().is_empty(),
                "a failure with nothing to type under it:\n  {line}"
            );
        }
    }
}

#[then("the answer is not empty")]
fn then_answer_not_empty(world: &mut Bohr) {
    let run = world.last();
    assert_eq!(run.code, 0, "{}", run.output);
    assert!(!run.output.trim().is_empty(), "nothing was printed");
}

/// Where the feature files live, relative to this crate.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn main() {
    // Scenarios that need a program on disk are only meaningful where one can be intercepted.
    // They are named rather than silently skipped, so a run on Windows says what it did not check.
    if !HAS_BACKEND {
        eprintln!(
            "note: scenarios tagged @linux are not run here -- this build has no interceptor, so \
             what follows is the half of the suite that needs none"
        );
    }
    futures::executor::block_on(
        Bohr::cucumber()
            .filter_run_and_exit(repo_root().join("tests/features"), |_, _, scenario| {
                HAS_BACKEND || !scenario.tags.iter().any(|tag| tag == "linux")
            }),
    );
}
