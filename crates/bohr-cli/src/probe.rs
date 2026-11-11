//! Finding the guests bohr supplies: the probe a scenario with `[[probe]]` or `[chain]` starts as
//! its last node, and the load a scenario with `[workload]` starts before it.
//!
//! Each is a guest, so it must be a static executable: a world has no dynamic loader. Looked for
//! in order: its variable, beside this binary, then where `scripts/build-probe.sh` puts it. A
//! dynamic one, such as the workspace build leaves, is skipped.

use std::path::{Path, PathBuf};

use crate::CliError;

/// The variable naming the probe executable, ahead of anywhere it is looked for.
pub(crate) const PROBE_VARIABLE: &str = "BOHR_PROBE";

/// The variable naming the load executable.
pub(crate) const LOAD_VARIABLE: &str = "BOHR_LOAD";

/// A guest bohr supplies: its file name and the variable that names it.
#[derive(Clone, Copy)]
struct Supplied {
    name: &'static str,
    variable: &'static str,
}

const PROBE: Supplied = Supplied { name: "bohr-probe", variable: PROBE_VARIABLE };
const LOAD: Supplied = Supplied { name: "bohr-load", variable: LOAD_VARIABLE };

/// The file the probe is executed from: a copy in the user's private directory, named by its
/// content.
pub(crate) fn program() -> Result<String, CliError> {
    // Once per process; the probe does not change during a campaign.
    static PLACED: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    PLACED.get_or_init(|| place(PROBE).map_err(|e| e.to_string())).clone().map_err(CliError::usage)
}

/// The file the load is executed from, placed as the probe is.
pub(crate) fn load_program() -> Result<String, CliError> {
    static PLACED: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    PLACED.get_or_init(|| place(LOAD).map_err(|e| e.to_string())).clone().map_err(CliError::usage)
}

/// The supplied guest a run knows by `argv0` inside it, where it is one.
pub(crate) fn supplied(argv0: &str) -> Option<Result<String, CliError>> {
    match argv0 {
        crate::PROBE_ARGV0 => Some(program()),
        crate::LOAD_ARGV0 => Some(load_program()),
        _ => None,
    }
}

fn place(guest: Supplied) -> Result<String, CliError> {
    let found = found(guest)?;
    let bytes = std::fs::read(&found)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {found}: {e}")))?;
    crate::private::place(guest.name.trim_start_matches("bohr-"), &bytes)
}

/// Where the guest is on this host, by the order the module names.
fn found(guest: Supplied) -> Result<String, CliError> {
    if let Ok(named) = std::env::var(guest.variable) {
        return match static_executable(Path::new(&named)) {
            Ok(()) => Ok(named),
            Err(why) => {
                Err(CliError::usage(format!("BOHR-1002: {}={named}: {why}", guest.variable)))
            }
        };
    }
    let here = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let candidates: [PathBuf; 2] = [
        here.join(guest.name),
        here.join("..").join("x86_64-unknown-linux-gnu").join("release").join(guest.name),
    ];
    let mut passed = Vec::new();
    for candidate in &candidates {
        match static_executable(candidate) {
            Ok(()) => return Ok(candidate.to_string_lossy().into_owned()),
            Err(why) => passed.push(format!("{}: {why}", candidate.display())),
        }
    }
    Err(CliError::usage(format!(
        "BOHR-1002: this scenario needs {}, and no static one was found ({}); build one with \
         `make probe`, or name one in {}",
        guest.name,
        passed.join("; "),
        guest.variable
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
