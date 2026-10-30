//! What `bohr replay` promises: a stored run comes back, or it says exactly where it did not.
//!
//! Every bundle here comes from a guest built into the binary, so these run on any operating
//! system with no privileges — which is also what makes such a bundle portable enough to be worth
//! carrying between machines.

use std::{path::PathBuf, process::Command};

fn bohr(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary must be runnable");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Writes the selftest bundles into a directory of this test's own and returns it.
fn bundles(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bohr-replay-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let (code, _, report) =
        bohr(&["selftest", "--repeat", "1", "--save-dir", &dir.to_string_lossy()]);
    assert_eq!(code, 0, "the selftest must produce bundles: {report}");
    dir
}

fn bundle_path(dir: &std::path::Path, guest: &str) -> String {
    dir.join(format!("{guest}.bohr")).to_string_lossy().into_owned()
}

#[test]
fn a_saved_run_replays_to_the_digest_it_recorded() {
    let dir = bundles("basic");
    for guest in ["clock-and-random", "two-tasks", "large-random"] {
        let path = bundle_path(&dir, guest);
        let (code, out, err) = bohr(&["replay", "--verify-only", &path]);
        assert_eq!(code, 0, "{guest}: {err}");
        assert!(out.contains("ok blake3:"), "{guest} said: {out}");
    }
}

/// Without `--verify-only` the replay says what the run was, not only that it matched: a bundle
/// arrives without the terminal that produced it, and the manifest is what is left to read.
#[test]
fn a_replay_reports_what_the_run_amounted_to() {
    let dir = bundles("report");
    let (code, out, err) = bohr(&["replay", &bundle_path(&dir, "two-tasks")]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("events"), "no counters in: {out}");
    assert!(out.contains("minimal"), "no provenance in: {out}");
}

/// A bundle whose recorded digest was edited must not replay to it — the check is the product.
#[test]
fn an_edited_digest_is_caught_and_the_divergence_is_named() {
    let dir = bundles("tamper");
    let path = bundle_path(&dir, "clock-and-random");
    let bytes = std::fs::read(&path).expect("read bundle");

    // The manifest is JSON inside a packed section, so the digest cannot be edited in place.
    // Rewriting the whole bundle is what a forger would have to do, and is what is done here.
    let bundle = bohr_trace::Bundle::parse(&bytes).expect("parse");
    let mut manifest = bundle.manifest().expect("manifest");
    manifest.trace_digest = "blake3:".to_owned() + &"00".repeat(32);

    let mut writer = bohr_trace::BundleWriter::new();
    writer.section(
        bohr_trace::SectionId::Manifest,
        serde_json::to_vec(&manifest).expect("manifest json"),
    );
    for id in bundle.section_ids() {
        if id != bohr_trace::SectionId::Manifest as u32 {
            writer.raw_section(id, bundle.raw_section(id).expect("section").to_vec());
        }
    }
    let forged = dir.join("forged.bohr");
    std::fs::write(&forged, writer.finish(bundle.run_id)).expect("write forged");

    let (code, _, err) = bohr(&["replay", "--verify-only", &forged.to_string_lossy()]);
    assert_ne!(code, 0, "a forged digest replayed clean");
    assert!(err.contains("BOHR-2011"), "the divergence must be named: {err}");
    assert!(err.contains("diverged at event") || err.contains("could not be read"), "{err}");
}

/// A bundle from another world model is refused before anything runs.
#[test]
fn a_foreign_world_model_is_refused() {
    let dir = bundles("provenance");
    let path = bundle_path(&dir, "large-random");
    let bytes = std::fs::read(&path).expect("read bundle");
    let bundle = bohr_trace::Bundle::parse(&bytes).expect("parse");
    let mut manifest = bundle.manifest().expect("manifest");
    manifest.provenance.world_model = bohr_types::WorldModelId::new("someone-elses", 3);

    let mut writer = bohr_trace::BundleWriter::new();
    writer.section(
        bohr_trace::SectionId::Manifest,
        serde_json::to_vec(&manifest).expect("manifest json"),
    );
    for id in bundle.section_ids() {
        if id != bohr_trace::SectionId::Manifest as u32 {
            writer.raw_section(id, bundle.raw_section(id).expect("section").to_vec());
        }
    }
    let foreign = dir.join("foreign.bohr");
    std::fs::write(&foreign, writer.finish(bundle.run_id)).expect("write foreign");

    let (code, _, err) = bohr(&["replay", "--verify-only", &foreign.to_string_lossy()]);
    assert_eq!(code, 2, "a foreign bundle is a refusal, not a finding: {err}");
    assert!(err.contains("BOHR-4006"), "{err}");
}

/// A bundle whose tape this build would read differently is refused with its own code.
#[test]
fn a_foreign_tape_abi_is_refused() {
    let dir = bundles("abi");
    let bytes = std::fs::read(bundle_path(&dir, "two-tasks")).expect("read bundle");
    let bundle = bohr_trace::Bundle::parse(&bytes).expect("parse");
    let mut manifest = bundle.manifest().expect("manifest");
    manifest.tape_abi += 1;

    let mut writer = bohr_trace::BundleWriter::new();
    writer.section(
        bohr_trace::SectionId::Manifest,
        serde_json::to_vec(&manifest).expect("manifest json"),
    );
    for id in bundle.section_ids() {
        if id != bohr_trace::SectionId::Manifest as u32 {
            writer.raw_section(id, bundle.raw_section(id).expect("section").to_vec());
        }
    }
    let newer = dir.join("newer-abi.bohr");
    std::fs::write(&newer, writer.finish(bundle.run_id)).expect("write");

    let (code, _, err) = bohr(&["replay", "--verify-only", &newer.to_string_lossy()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOHR-4005"), "{err}");
}

/// Several bundles are checked in one invocation, and one bad one does not hide the rest.
#[test]
fn a_batch_reports_every_bundle_and_the_worst_outcome() {
    let dir = bundles("batch");
    let good = bundle_path(&dir, "clock-and-random");
    let missing = dir.join("absent.bohr").to_string_lossy().into_owned();

    let (code, out, err) = bohr(&["replay", "--verify-only", &good, &missing]);
    assert_ne!(code, 0, "a missing bundle must not pass");
    assert!(out.contains("ok blake3:"), "the good bundle was not checked: {out}");
    assert!(err.contains("absent.bohr"), "the missing one was not named: {err}");
}

/// A cluster run saved by `bohr run` replays to the digest it recorded.
///
/// The regression this exists for was a two-variable difference in the environment: `bohr run`
/// built its guest specifications one way and `bohr replay` another, so the guest's stack sat
/// sixty-four bytes apart in the two invocations and any guest reading a stack address replayed to
/// a different digest than it recorded. Nothing caught it, because the determinism gate compares
/// runs against each other inside one invocation and never a run against its own replay.
#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn a_cluster_run_replays_to_what_it_recorded() {
    let guest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/guests/build/clock_and_random");
    let Ok(guest) = guest.canonicalize() else {
        return;
    };

    let dir = std::env::temp_dir().join("bohr-replay-cluster");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory of this test's own");
    let config = dir.join("scenario.toml");
    std::fs::write(
        &config,
        format!("[cluster]\nnodes = 1\nargv = [{:?}]\n", guest.to_string_lossy()),
    )
    .expect("the scenario is written");

    let saved = dir.join("run.bohr");
    let (code, out, report) = bohr(&[
        "run",
        "--seed",
        "1",
        "--config",
        &config.to_string_lossy(),
        "--save",
        &saved.to_string_lossy(),
        "--",
        &guest.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{out}{report}");

    let (code, out, report) = bohr(&["replay", "--verify-only", &saved.to_string_lossy()]);
    assert_eq!(code, 0, "a cluster run must replay to what it recorded: {out}{report}");
    assert!(format!("{out}{report}").contains("ok blake3:"), "{out}{report}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A run saved with arguments comes back with them.
///
/// **The bundle recorded the program and not what it was started with.** Arguments sit on the
/// stack a guest is born with, so replaying without them moves everything above them — the run
/// module records two environment variables shifting a stack by sixty-four bytes and breaking a
/// digest, and missing arguments shift it the same way. A bundle that cannot replay reports
/// BOHR-2011 against the tool that wrote it, which is a defect in this tool by its own taxonomy.
///
/// Needs a real program to pass arguments to, so it runs where one can be traced.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn a_run_saved_with_arguments_replays_with_them() {
    let guest = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/guests/build/cat");
    if !std::path::Path::new(guest).exists() {
        eprintln!("skipped: the guests are not built");
        return;
    }

    let dir = std::env::temp_dir().join("bohr-replay-argv");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let bundle = dir.join("with-args.bohr");

    // A name the modelled filesystem does not hold, so the guest fails to open it and says so --
    // a run like any other and exactly as reproducible. What is under test is that the argument
    // reaches the guest at all: without it the guest reads its standard input instead, takes a
    // different path and records a different digest.
    let saved = bundle.to_string_lossy().into_owned();
    let (code, _, said) = bohr(&["run", "--seed", "5", "--save", &saved, "--", guest, "/absent"]);
    assert!(code == 0 || code == 1, "the run did not complete: {code} {said}");

    // The claim: the same bundle replays to the digest it recorded. Without the arguments the
    // guest reads no file, the stack differs, and this is where it says so.
    let (code, out, said) = bohr(&["replay", &bundle.to_string_lossy()]);
    assert_eq!(code, 0, "the bundle did not replay: {out}{said}");
    assert!(!said.contains("BOHR-2011"), "the replay diverged from what was recorded: {said}");
}
