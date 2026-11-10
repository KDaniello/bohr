//! What `bohr survey` reports, and what it does not claim.
//!
//! A survey answers each unmodelled call with `ENOSYS` and carries on, so the unsupported set comes
//! back in one pass. Those answers are invented, so its caveats are tested as much as its list.

use std::process::Command;

/// Whether a guest can be intercepted here at all.
const HAS_BACKEND: bool = cfg!(all(target_os = "linux", target_arch = "x86_64"));

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary must be runnable");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

/// A guest built by `make guests`, if built.
fn guest(name: &str) -> Option<String> {
    if !HAS_BACKEND {
        return None;
    }
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/guests/build")
        .join(name);
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// A survey reports the call that a run refuses.
#[test]
fn a_survey_names_what_the_run_would_only_refuse() {
    let Some(path) = guest("enters_io_uring") else {
        return;
    };

    let (refused, _) = bohr(&["run", "--seed", "1", "--", &path]);
    assert_eq!(refused, 3, "an ordinary run cannot carry this program");

    let (code, out) = bohr(&["survey", "--", &path]);
    assert_eq!(code, 4, "a survey that found something is neither clean nor a finding:\n{out}");
    assert!(out.contains("426"), "the number must be reported:\n{out}");
    assert!(out.contains("io_uring"), "and named, because a number is not a name:\n{out}");
}

/// The report states beside the list that it is a lower bound.
#[test]
fn a_survey_says_what_it_is_not_evidence_of() {
    let Some(path) = guest("enters_io_uring") else {
        return;
    };
    let (_, out) = bohr(&["survey", "--", &path]);

    assert!(out.contains("lower bound"), "the report must bound its own claim:\n{out}");
    assert!(out.contains("fallback"), "and say why the guest's path changed:\n{out}");
}

/// A fully modelled program surveys clean with exit 0.
#[test]
fn a_program_this_build_can_carry_surveys_clean() {
    let Some(path) = guest("startup") else {
        return;
    };
    let (code, out) = bohr(&["survey", "--", &path]);

    assert_eq!(code, 0, "nothing was unmodelled, so nothing is reported:\n{out}");
    assert!(out.contains("every call it made was answered"), "{out}");
}

/// A survey with no program is a usage error.
#[test]
fn a_survey_of_nothing_is_refused() {
    let (code, out) = bohr(&["survey"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("BOHR-1002"), "{out}");
}

/// A guest that never finished has no surface and the survey does not exit clean.
///
/// A dynamic program whose loader finds no libraries makes a dozen answered calls and exits 127
/// before its first instruction.
#[test]
fn a_guest_that_never_finished_is_not_a_clean_surface() {
    let Some(path) = guest("segfault") else {
        return;
    };
    let (code, out) = bohr(&["survey", "--", &path]);

    assert_ne!(code, 0, "a survey of a guest that died must not report clean:\n{out}");
    assert!(out.contains("did not run to completion"), "and must say which guest state:\n{out}");
}
