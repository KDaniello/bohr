//! Every bundle in `testdata/bundles/` must still replay.
//!
//! This is the promise that a finding stored today is still reproducible after the tool has moved
//! on (`docs/TESTING.md` §7.2). Adding to the collection is mandatory whenever a determinism bug
//! is fixed: the bundle that caught it becomes the test that it stays caught.

use std::{path::PathBuf, process::Command};

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bundles")
}

#[test]
fn every_golden_bundle_still_replays() {
    let dir = corpus();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        panic!("the golden corpus is missing at {}", dir.display());
    };

    let mut checked = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "bohr") {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();

        // A bundle produced under interception can only be replayed where a guest can be traced,
        // and where its recorded binary is still on disk. Skipping is honest; pretending it
        // passed is not, so the skip is printed.
        if name.starts_with("traced-") && !can_trace() {
            eprintln!("{name}: skipped, this host cannot trace a guest");
            continue;
        }

        let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
            .args(["replay", "--verify-only", &path.to_string_lossy()])
            .output()
            .expect("the binary must be runnable");
        assert_eq!(
            out.status.code().unwrap_or(-1),
            0,
            "{name} no longer replays:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        checked += 1;
    }

    assert!(checked > 0, "the golden corpus replayed nothing; is it empty?");
}

const fn can_trace() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}
