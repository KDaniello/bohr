//! `bohr hunt` against a pack made for the test: one command, a pinned binary, a scenario.
//!
//! Linux only; skips when the guest is not built.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{path::PathBuf, process::Command};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bohr(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .args(args)
        .output()
        .expect("the binary must be runnable");
    let mut report = String::from_utf8_lossy(&out.stdout).into_owned();
    report.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), report)
}

/// A repository holding one pack, `targets/demo`, whose binary is a built guest.
fn repository(tag: &str, pinned: Option<&str>) -> Option<PathBuf> {
    let guest = root().join("testdata/guests/build/file_write_sync");
    if !guest.is_file() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("bohr-hunt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("targets/demo")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let bytes = std::fs::read(&guest).unwrap();
    std::fs::write(dir.join("bin/demo"), &bytes).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("bin/demo"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    std::fs::write(
        dir.join("demo.toml"),
        format!(
            "[cluster]\nnodes = 1\nargv = [{:?}]\n[faults]\nkill_probability = \"1/50\"\n",
            dir.join("bin/demo").to_string_lossy()
        ),
    )
    .unwrap();
    let digest = pinned.map_or_else(|| bohr_types::Digest::of(&bytes).to_hex(), str::to_owned);
    std::fs::write(
        dir.join("targets/demo/target.toml"),
        format!(
            "[source]\nrepository = \"https://example.org/demo\"\ncommit = \"0\"\n\
             [build]\ncommand = [\"true\"]\nbinary = \"bin/demo\"\nblake3 = \"{digest}\"\n\
             [run]\nscenario = \"demo.toml\"\n[judge]\noracle = \"crash\"\n"
        ),
    )
    .unwrap();
    Some(dir)
}

/// One command runs the pack's scenario against its pinned binary, over seeds or as a search.
#[test]
fn a_pack_is_hunted_in_one_command() {
    let Some(dir) = repository("one", None) else { return };
    let pack = dir.join("targets/demo").to_string_lossy().into_owned();
    let out = dir.join("out").to_string_lossy().into_owned();
    let (code, report) = bohr(&["hunt", &pack, "--seeds", "1..4", "-j", "2", "-o", &out]);
    assert!(matches!(code, 0 | 1), "{report}");
    assert!(report.contains("swept 1..4"), "{report}");

    let searched = dir.join("searched").to_string_lossy().into_owned();
    let (code, report) = bohr(&["hunt", &pack, "--runs", "8", "-o", &searched]);
    assert!(matches!(code, 0 | 1), "{report}");
    assert!(dir.join("searched/summary.json").is_file(), "{report}");
    assert!(dir.join("searched/campaign/campaign.bin").is_file(), "it can be resumed");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A binary whose digest differs from the pack's stops the hunt before any run, naming both.
#[test]
fn a_changed_binary_is_refused_before_it_runs() {
    let Some(dir) = repository("changed", Some("00")) else { return };
    let pack = dir.join("targets/demo").to_string_lossy().into_owned();
    let out = dir.join("out");
    let (code, report) = bohr(&["hunt", &pack, "--runs", "4", "-o", &out.to_string_lossy()]);
    assert_eq!(code, 2, "{report}");
    assert!(report.contains("names 00"), "{report}");
    assert!(!out.join("summary.json").exists(), "nothing ran");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A given binary with the pack's digest runs in place of the pack's own, even when that is gone.
#[test]
fn a_given_binary_is_the_one_that_runs() {
    let Some(dir) = repository("given", None) else { return };
    let elsewhere = dir.join("elsewhere");
    std::fs::rename(dir.join("bin/demo"), &elsewhere).unwrap();
    let pack = dir.join("targets/demo").to_string_lossy().into_owned();
    let out = dir.join("out").to_string_lossy().into_owned();
    let (code, report) = bohr(&[
        "hunt",
        &pack,
        "--seeds",
        "1..2",
        "--binary",
        &elsewhere.to_string_lossy(),
        "-o",
        &out,
    ]);
    assert!(matches!(code, 0 | 1), "{report}");
    assert!(report.contains("swept 1..2") && !report.contains("did not complete,"), "{report}");
    assert!(
        !report.contains("1 that did not complete") && !report.contains("2 that did not"),
        "{report}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
