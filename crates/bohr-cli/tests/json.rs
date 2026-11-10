//! `--json` held to golden files: a schema's output changes only together with its version.
//!
//! Each case runs in its own directory of bundle copies, so paths are relative and host-neutral.
//! Any difference fails; after a version bump, `BOHR_BLESS=1` rewrites the golden file.
//!
//! Commands that need a backend are checked by the Linux tests that run them.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use bohr_trace::{Bundle, Outcome, SectionId};
use serde_json::Value;

const BUNDLES: [&str; 3] =
    ["clock-and-random-seed0.bohr", "large-random-seed0.bohr", "two-tasks-seed0.bohr"];

/// A directory of copies of the stored bundles, removed when dropped.
struct Here(PathBuf);

impl Here {
    /// The stored bundles under `findings/`, with `more` written beside them.
    fn new(case: &str, more: &[(&str, Vec<u8>)]) -> Self {
        let dir = std::env::temp_dir().join(format!("bohr-json-{case}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("findings")).expect("a directory");
        let stored = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bundles");
        for name in BUNDLES {
            std::fs::copy(stored.join(name), dir.join("findings").join(name)).expect("a copy");
        }
        for (name, bytes) in more {
            std::fs::write(dir.join("findings").join(name), bytes).expect("a bundle");
        }
        Self(dir)
    }
}

impl Drop for Here {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A finding for `bohr findings` to group: the stored bundles all ran clean, and no built-in guest
/// fails, so the two-task run is recorded as ended by a crash.
fn crashed() -> Vec<u8> {
    let stored =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bundles/two-tasks-seed0.bohr");
    let bytes = std::fs::read(stored).expect("the stored bundle");
    let bundle = Bundle::parse(&bytes).expect("a bundle");
    let mut manifest = bundle.manifest().expect("a manifest");
    manifest.outcome = Outcome::GuestCrashed("killed by signal 11".to_owned());
    let mut writer = bundle.to_writer();
    writer.section(SectionId::Manifest, serde_json::to_vec(&manifest).expect("a manifest"));
    writer.finish(bundle.run_id)
}

/// The one object `args` writes, with the exit code; stdout holds nothing else.
fn object(case: &str, args: &[&str], more: &[(&str, Vec<u8>)]) -> (Value, i32) {
    let here = Here::new(case, more);
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .arg("--json")
        .args(args)
        .current_dir(&here.0)
        .output()
        .expect("the binary under test must be runnable");
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "{case}: standard output is not one object:\n{text}");
    let value = serde_json::from_str(lines[0]).unwrap_or_else(|e| panic!("{case}: {e}:\n{text}"));
    (value, out.status.code().unwrap_or(-1))
}

/// Normalises the version and the path separator, which vary by build and host.
fn steady(mut value: Value) -> Value {
    fn separators(value: &mut Value) {
        match value {
            Value::String(text) => *text = text.replace('\\', "/"),
            Value::Array(items) => items.iter_mut().for_each(separators),
            Value::Object(fields) => fields.values_mut().for_each(separators),
            _ => {}
        }
    }
    separators(&mut value);
    if let Some(version) = value.get_mut("tool_version") {
        *version = Value::String("<this build>".to_owned());
    }
    value
}

fn golden(case: &str, args: &[&str], exit: i32) {
    golden_with(case, args, exit, &[]);
}

/// [`golden`], with bundles written beside the stored ones.
fn golden_with(case: &str, args: &[&str], exit: i32, more: &[(&str, Vec<u8>)]) {
    let (value, code) = object(case, args, more);
    assert_eq!(code, exit, "{case}: exit {code}: {value}");
    let value = steady(value);
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/json/{case}.json"));
    let written = serde_json::to_string_pretty(&value).expect("printable") + "\n";
    if std::env::var_os("BOHR_BLESS").is_some() {
        std::fs::create_dir_all(file.parent().expect("a parent")).expect("a directory");
        std::fs::write(&file, &written).expect("a golden file");
        return;
    }
    let kept = std::fs::read_to_string(&file).unwrap_or_else(|_| {
        panic!("{case}: no golden file at {}; write it with BOHR_BLESS=1", file.display())
    });
    if kept.replace("\r\n", "\n") == written {
        return;
    }
    let before: Value = serde_json::from_str(&kept).expect("a golden file is JSON");
    if before["schema"] == value["schema"] {
        panic!(
            "{case}: the output of {} changed without a new schema version. Bump it in \
             crates/bohr-cli/src/json.rs, or put the output back.\nwas:\n{kept}\nnow:\n{written}",
            value["schema"]
        );
    }
    panic!(
        "{case}: {} is now {}; write the golden file again with BOHR_BLESS=1",
        before["schema"], value["schema"]
    );
}

#[test]
fn version() {
    golden("version", &["version"], 0);
}

#[test]
fn explain() {
    golden("explain", &["explain", "findings/two-tasks-seed0.bohr"], 0);
}

#[test]
fn trace() {
    golden("trace", &["trace", "findings/two-tasks-seed0.bohr", "--around", "8"], 0);
}

#[test]
fn findings() {
    golden_with("findings", &["findings", "findings"], 0, &[("crashed.bohr", crashed())]);
}

#[test]
fn note() {
    golden("note", &["note", "findings/two-tasks-seed0.bohr", "seen on a laptop"], 0);
}

#[test]
fn report() {
    golden("report", &["report", "findings/two-tasks-seed0.bohr", "-o", "page.html"], 0);
}

#[test]
fn error() {
    golden("error", &["run", "--seed", "1", "--tape", "t.tape", "--", "x"], 2);
}
