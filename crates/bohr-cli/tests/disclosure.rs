//! `bohr report --disclosure` and `--sarif`: what a maintainer receives, and that it can be acted
//! on.
//!
//! The finding comes from `lease_clock`, which fails on every seed because its clock step is
//! scheduled. That half is Linux only and skips when the guest is not built.

use std::{path::PathBuf, process::Command};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bohr(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .current_dir(root())
        .args(args)
        .output()
        .expect("the binary under test must be runnable");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bohr-disclosure-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory of this test's own");
    dir
}

/// A clean run has nothing to disclose and is refused.
#[test]
fn a_clean_run_is_refused_with_the_reason() {
    let out = scratch("clean").join("report.md");
    let (code, _, err) = bohr(&[
        "report",
        "testdata/bundles/two-tasks-seed0.bohr",
        "--disclosure",
        "-o",
        &out.to_string_lossy(),
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-5003"), "{err}");
    assert!(!out.exists(), "a draft was written for a run that found nothing");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod real {
    use super::*;

    const GUEST: &str = "testdata/guests/build/lease_clock";

    /// Saves one seed's finding, or nothing if the guest is not built.
    fn found(dir: &std::path::Path, seed: &str) -> Option<String> {
        if !root().join(GUEST).is_file() {
            return None;
        }
        let path = dir.join(format!("lease-{seed}.bohr")).to_string_lossy().into_owned();
        let (code, _, err) = bohr(&[
            "run",
            "--seed",
            seed,
            "--config",
            "testdata/bugs/lease_clock/scenario.toml",
            "--save",
            &path,
            "--",
            GUEST,
        ]);
        assert_eq!(code, 1, "the stepped clock must be found on seed {seed}: {err}");
        Some(path)
    }

    /// The draft carries the command and the digest, and the command, run, reproduces.
    #[test]
    fn a_draft_names_a_command_that_reproduces_and_the_digest_it_reaches() {
        let dir = scratch("draft");
        let Some(bundle) = found(&dir, "1") else { return };
        let out = dir.join("report.md");
        let (code, said, err) =
            bohr(&["report", &bundle, "--disclosure", "-o", &out.to_string_lossy()]);
        assert_eq!(code, 0, "{err}");
        assert!(said.contains("report.md"), "{said}");

        let draft = std::fs::read_to_string(&out).expect("the draft was written");
        let command = format!("bohr replay {bundle}");
        assert!(draft.contains(&command), "no reproduction command:\n{draft}");
        let digest = bohr_trace::Bundle::parse(&std::fs::read(&bundle).expect("the bundle"))
            .expect("parses")
            .manifest()
            .expect("a manifest")
            .trace_digest;
        assert!(draft.contains(&digest), "no digest:\n{draft}");
        for heading in ["## Target", "## What went wrong", "## How to reproduce", "## What was run"]
        {
            assert!(draft.contains(heading), "no {heading:?}:\n{draft}");
        }
        assert!(draft.contains("## What this cannot claim"), "no limits:\n{draft}");

        let (code, said, err) = bohr(&["replay", &bundle]);
        assert!(said.contains(&digest), "the replay did not reach the digest: {said}{err}");
        assert_ne!(code, 2, "the command in the draft was refused: {err}");
    }

    /// Two seeds of one defect are one result, led by the smaller, with a location for each.
    #[test]
    fn two_runs_of_one_defect_are_one_sarif_result() {
        let dir = scratch("sarif");
        let findings = dir.join("findings");
        std::fs::create_dir_all(&findings).expect("a directory");
        let (Some(_), Some(_)) = (found(&findings, "1"), found(&findings, "2")) else { return };
        let out = dir.join("finding.sarif");
        let (code, said, err) =
            bohr(&["report", &findings.to_string_lossy(), "--sarif", "-o", &out.to_string_lossy()]);
        assert_eq!(code, 0, "{err}");
        assert!(said.contains("1 result"), "{said}");

        let log: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).expect("written"))
                .expect("SARIF is JSON");
        assert_eq!(log["version"], "2.1.0");
        let results = log["runs"][0]["results"].as_array().expect("results");
        assert_eq!(results.len(), 1, "one defect became {} results", results.len());
        assert_eq!(results[0]["properties"]["bundles"], 2);
        let rule = &log["runs"][0]["tool"]["driver"]["rules"][0]["id"];
        assert_eq!(&results[0]["ruleId"], rule);
        assert!(
            results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"].is_string()
        );
    }
}
