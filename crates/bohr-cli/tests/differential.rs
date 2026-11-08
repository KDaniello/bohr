//! One tape, several implementations, and what a comparison is worth.
//!
//! Two programs implementing one specification are handed the same source of randomness. Neither
//! crashes, neither asserts anything, and both exit zero — so nothing but comparing what they
//! produced could tell them apart, which is the point: a real client offers a tool no assertion and
//! no bad exit status to work with either.

use std::{path::PathBuf, process::Command};

/// Whether a guest can be intercepted here at all.
const HAS_BACKEND: bool = cfg!(all(target_os = "linux", target_arch = "x86_64"));

/// Run from the repository root, because a scenario names its implementations the way somebody
/// typing the command would: relative to where they are standing.
fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .current_dir(root())
        .output()
        .expect("the binary must be runnable");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scenario file, where the guests it names have been built.
fn scenario(name: &str) -> Option<String> {
    if !HAS_BACKEND {
        return None;
    }
    for guest in ["answers_from_draw", "answers_from_draw_wrong"] {
        if !root().join("testdata/guests/build").join(guest).is_file() {
            return None;
        }
    }
    Some(root().join("testdata/differential").join(name).to_string_lossy().into_owned())
}

/// Implementations that agree are not a finding, and the run says what it checked.
#[test]
fn implementations_that_agree_are_clean() {
    let Some(config) = scenario("agree.toml") else {
        return;
    };
    let (code, out) = bohr(&["run", "--seed", "1", "--config", &config]);

    assert_eq!(code, 0, "{out}");
    assert!(out.contains("agreed on every projection"), "{out}");
}

/// The seeded disagreement, found by nothing but the comparison.
#[test]
fn implementations_that_differ_are_a_finding() {
    let Some(config) = scenario("disagree.toml") else {
        return;
    };
    let (code, out) = bohr(&["run", "--seed", "1", "--config", &config]);

    assert_eq!(code, 1, "a disagreement is a finding, which is a success for the tool: {out}");
    assert!(out.contains("disagreed"), "{out}");
    assert!(out.contains("answer"), "the projection they differed on must be named: {out}");
    assert!(out.contains("reference") && out.contains("second"), "both must be named: {out}");
    // Neither program did anything wrong on its own, which is exactly why this needed comparing.
    assert!(!out.contains("crashed"), "{out}");
}

/// The trap. A run that compared nothing must never look clean.
///
/// A campaign of a million differential runs with a projection path nobody writes would otherwise
/// report a million clean runs while checking nothing at all — and every one of them would look
/// exactly like a run that passed.
#[test]
fn a_run_that_compared_nothing_is_not_clean() {
    let Some(config) = scenario("agree.toml") else {
        return;
    };
    let text = std::fs::read_to_string(&config).expect("the scenario is readable");
    let broken = text.replace("file:/answer", "file:/nobody-writes-this");
    let path = std::env::temp_dir().join("bohr-differential-nothing.toml");
    std::fs::write(&path, broken).expect("a scenario of this test's own");

    let (code, out) = bohr(&["run", "--seed", "1", "--config", &path.to_string_lossy()]);
    assert_ne!(code, 0, "a run that compared nothing is not a clean run: {out}");
    assert!(out.contains("BOHR-5004"), "{out}");

    let _ = std::fs::remove_file(&path);
}

/// A comparison needs something to compare: one implementation is a run, not a comparison.
#[test]
fn a_differential_scenario_with_one_implementation_is_refused() {
    if !HAS_BACKEND {
        return;
    }
    let path = std::env::temp_dir().join("bohr-differential-alone.toml");
    std::fs::write(
        &path,
        "[differential]\nprojections = [{ label = \"a\", source = \"stdout\" }]\n\n\
         [[differential.implementation]]\nname = \"only\"\nargv = [\"/bin/true\"]\n",
    )
    .expect("a scenario of this test's own");

    let (code, out) = bohr(&["run", "--seed", "1", "--config", &path.to_string_lossy()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("BOHR-1002"), "{out}");

    let _ = std::fs::remove_file(&path);
}

/// The shape a real client is compared in: each implementation shares a cluster with its caller.
///
/// An execution client does nothing until something drives it, so every node's program comes from
/// `[cluster]`. The danger is both implementations running the same program: the run reports
/// agreement having executed one program twice, and nothing about it looks wrong.
#[test]
fn an_implementation_replaces_its_own_node_of_a_cluster() {
    let Some(config) = scenario("cluster.toml") else {
        return;
    };
    let (code, out) = bohr(&["run", "--seed", "1", "--config", &config]);

    assert_eq!(code, 1, "the two implementations differ and the cluster must not hide it: {out}");
    assert!(out.contains("disagreed"), "{out}");
    assert!(out.contains("answer"), "the projection they differed on must be named: {out}");
}

/// The node under comparison is not the node holding the answer, and the scenario says which.
///
/// A client is compared by what it answered, and what it answered is written down by the caller
/// that asked. Reading node zero by convention read the client's own disk, where this value is not:
/// the comparison found nothing and was skipped, having looked in the wrong place. Node zero here
/// holds an answer of its own, and a deliberately different one per implementation, so a run that
/// read the wrong node would report a disagreement rather than merely finding nothing.
#[test]
fn a_projection_is_read_off_the_node_that_wrote_it() {
    let Some(config) = scenario("driven.toml") else {
        return;
    };
    let (code, out) = bohr(&["run", "--seed", "1", "--config", &config]);

    assert_eq!(code, 0, "node one agrees with itself, whatever node zero was running: {out}");
    assert!(out.contains("agreed on every projection"), "{out}");
}

/// Drawn faults draw per syscall, and two implementations make different syscalls, so a drawn
/// schedule cannot be the same for both. Refused rather than silently compared under unequal
/// conditions.
#[test]
fn drawn_faults_are_refused_alongside_a_comparison() {
    let Some(config) = scenario("agree.toml") else {
        return;
    };
    let text = std::fs::read_to_string(&config).expect("the scenario is readable");
    let path = std::env::temp_dir().join("bohr-differential-drawn.toml");
    std::fs::write(&path, format!("{text}\n[faults]\nkill_probability = \"1/100\"\n"))
        .expect("a scenario of this test's own");

    let (code, out) = bohr(&["run", "--seed", "1", "--config", &path.to_string_lossy()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("kill_probability"), "the refusal must name what is wrong: {out}");

    let _ = std::fs::remove_file(&path);
}
