//! `bohr explore`, and the promise that the worker count cannot be seen from the outside.
//!
//! The campaign's own reproducibility is settled in `bohr-explorer` and proven against the
//! scripted backend by `bohr-testkit`; what is left to check here is the plumbing — that splitting
//! a generation across processes, handing work over and reading results back does not smuggle the
//! machine into the answer.
//!
//! Searched against a guest built into the binary, so this runs wherever the tool builds.

use std::{path::PathBuf, process::Command};

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary under test must be runnable");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

/// A directory of this test's own, emptied first.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bohr-explore-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory of this test's own");
    dir
}

/// Runs a campaign and returns its summary, which is everything two campaigns must agree on.
fn campaign(tag: &str, jobs: &str, runs: &str, seed: &str) -> String {
    let dir = scratch(tag);
    let summary = dir.join("summary.json");
    let (code, report) = bohr(&[
        "explore",
        "--guest",
        "many-flips",
        "--runs",
        runs,
        "--master-seed",
        seed,
        "-j",
        jobs,
        "--findings",
        &dir.join("findings").to_string_lossy(),
        "--summary",
        &summary.to_string_lossy(),
    ]);
    assert!(matches!(code, 0 | 1), "the campaign did not complete: {report}");
    std::fs::read_to_string(&summary).expect("a campaign writes the summary it was asked for")
}

/// The milestone gate. One master seed, one run budget, any number of workers — one campaign.
#[test]
fn one_worker_and_eight_search_the_same_thing() {
    let alone = campaign("j1", "1", "128", "11");
    let crowd = campaign("j8", "8", "128", "11");
    assert_eq!(alone, crowd, "the worker count reached the result");
}

/// And a campaign is repeatable at all, which is the weaker claim the stronger one rests on.
#[test]
fn one_master_seed_is_one_campaign() {
    assert_eq!(campaign("again-a", "4", "96", "3"), campaign("again-b", "4", "96", "3"));
}

/// A different seed has to search somewhere else, or the master seed is decoration.
#[test]
fn a_different_master_seed_searches_differently() {
    assert_ne!(campaign("seed-a", "2", "96", "1"), campaign("seed-b", "2", "96", "2"));
}

/// A budget nobody gave is a campaign that would never end. Refused at the door rather than
/// defaulted to something the caller did not choose.
#[test]
fn a_campaign_without_a_budget_is_refused() {
    let (code, report) = bohr(&["explore", "--guest", "many-flips"]);
    assert_eq!(code, 2, "{report}");
    assert!(report.contains("BOHR-1001"), "the refusal must name its code: {report}");
    assert!(report.contains("--runs"), "and say what to type: {report}");
}

/// The run budget is exact. A campaign asked for ninety-six runs that did a hundred and twenty
/// would be a campaign nobody could repeat by asking for the same thing.
#[test]
fn a_run_budget_is_met_exactly() {
    let summary = campaign("exact", "3", "50", "5");
    assert!(summary.contains(r#""runs": 50"#), "the budget was not met exactly: {summary}");
}

/// A campaign that armed adversity and landed none has measured a guest under no pressure.
///
/// **The failure this catches is silent and it happened.** An execution client's campaign armed a
/// kill three hundred milliseconds in and ended with its caller at forty-four; twenty seeds fired
/// nothing, built their blocks, and reported twenty clean runs. A horizon does not bound the end of
/// a run that `run.ends_with_node` decides, so no check at load could have known — only the runs
/// can say, and this is them saying it.
#[test]
fn a_campaign_that_armed_adversity_and_landed_none_is_not_clean() {
    let dir = scratch("landed-nothing");
    let config = dir.join("armed-too-late.toml");
    std::fs::write(
        &config,
        "[run]\nhorizon_ticks = 1_000_000_000\n\n\
         [cluster]\nnodes = 2\n\n\
         [faults]\nkill_probability = \"1/2\"\narm_after_ticks = 999_000_000\n",
    )
    .expect("a scenario of this test's own");

    let (code, report) = bohr(&[
        "explore",
        "--guest",
        "many-flips",
        "--runs",
        "8",
        "--config",
        &config.to_string_lossy(),
        "--findings",
        &dir.join("findings").to_string_lossy(),
    ]);
    assert_eq!(code, 3, "a campaign that pressed on nothing is not a clean campaign: {report}");
    assert!(report.contains("BOHR-5006"), "the refusal must name its code: {report}");
    assert!(report.contains("ends_with_node"), "and where to look: {report}");
}

/// Naming both a built-in guest and a program is a question with no answer, and guessing at one
/// would search something the caller did not ask about.
#[test]
fn naming_two_subjects_is_refused() {
    let (code, report) =
        bohr(&["explore", "--guest", "many-flips", "--runs", "8", "--", "/bin/true"]);
    assert_eq!(code, 2, "{report}");
    assert!(report.contains("not both"), "{report}");
}

/// Runs a campaign that writes itself down in `state`, and returns its summary.
fn resumable(dir: &std::path::Path, runs: &str, seed: &str, jobs: &str) -> (i32, String) {
    let summary = dir.join(format!("summary-{runs}.json"));
    let (code, report) = bohr(&[
        "explore",
        "--guest",
        "many-flips",
        "--runs",
        runs,
        "--master-seed",
        seed,
        "-j",
        jobs,
        "--findings",
        &dir.join("findings").to_string_lossy(),
        "--resume",
        &dir.join("state").to_string_lossy(),
        "--summary",
        &summary.to_string_lossy(),
    ]);
    (code, std::fs::read_to_string(&summary).unwrap_or(report))
}

/// Stopped at a run budget that cuts a generation short and resumed to a larger one, a
/// campaign is the campaign that ran the larger budget at once, byte for byte, whatever the
/// worker count on either side of the stop.
#[test]
fn a_resumed_campaign_is_the_same_campaign() {
    let whole = campaign("whole", "1", "100", "21");
    let dir = scratch("resumed");
    let (code, _) = resumable(&dir, "50", "21", "1");
    assert!(matches!(code, 0 | 1));
    let (code, resumed) = resumable(&dir, "100", "21", "4");
    assert!(matches!(code, 0 | 1), "{resumed}");
    assert_eq!(whole, resumed, "the stop reached the result");
}

/// A directory holding another campaign is refused rather than continued as this one.
#[test]
fn a_resume_of_another_campaign_is_refused() {
    let dir = scratch("other");
    let (code, _) = resumable(&dir, "40", "3", "1");
    assert!(matches!(code, 0 | 1));
    let (code, report) = resumable(&dir, "80", "4", "1");
    assert_eq!(code, 2, "{report}");
    assert!(report.contains("master seed 3"), "{report}");
}
