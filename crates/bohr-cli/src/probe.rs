//! Finding the probe a scenario with `[[probe]]` or `[chain]` starts as its last node.
//!
//! The probe is a guest, so it must be a static executable: a world has no dynamic loader. Looked
//! for in order: the variable, beside this binary, then where `scripts/build-probe.sh` puts it. A
//! dynamic one, such as the workspace build leaves, is skipped.

use std::path::{Path, PathBuf};

use crate::CliError;

/// The variable naming the probe executable, ahead of anywhere it is looked for.
pub(crate) const PROBE_VARIABLE: &str = "BOHR_PROBE";

/// The file the probe is executed from: a copy in the user's private directory, named by its
/// content.
pub(crate) fn program() -> Result<String, CliError> {
    // Once per process; the probe does not change during a campaign.
    static PLACED: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    PLACED.get_or_init(|| place().map_err(|e| e.to_string())).clone().map_err(CliError::usage)
}

fn place() -> Result<String, CliError> {
    let found = found()?;
    let bytes = std::fs::read(&found)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {found}: {e}")))?;
    crate::private::place("probe", &bytes)
}

/// The probe this host has, by the order the module names.
fn found() -> Result<String, CliError> {
    if let Ok(named) = std::env::var(PROBE_VARIABLE) {
        return match static_executable(Path::new(&named)) {
            Ok(()) => Ok(named),
            Err(why) => Err(CliError::usage(format!("BOHR-1002: {PROBE_VARIABLE}={named}: {why}"))),
        };
    }
    let here = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let candidates: [PathBuf; 2] = [
        here.join("bohr-probe"),
        here.join("..").join("x86_64-unknown-linux-gnu").join("release").join("bohr-probe"),
    ];
    let mut passed = Vec::new();
    for candidate in &candidates {
        match static_executable(candidate) {
            Ok(()) => return Ok(candidate.to_string_lossy().into_owned()),
            Err(why) => passed.push(format!("{}: {why}", candidate.display())),
        }
    }
    Err(CliError::usage(format!(
        "BOHR-1002: this scenario has probes, and no static bohr-probe was found ({}); build one \
         with `make probe`, or name one in {PROBE_VARIABLE}",
        passed.join("; ")
    )))
}

/// Whether `path` is an executable a world can start: present, and asking for no loader.
fn static_executable(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.kind().to_string())?;
    match crate::init::interpreter(&bytes) {
        Some(loader) => Err(format!("dynamic, it asks for {loader}")),
        None if bytes.starts_with(b"\x7fELF") => Ok(()),
        None => Err("not an executable".to_owned()),
    }
}
