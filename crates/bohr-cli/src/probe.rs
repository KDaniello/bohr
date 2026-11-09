//! Finding the probe a scenario with `[[probe]]` or `[chain]` starts as its last node.
//!
//! The probe is a guest, so it is a file the world executes and not code in this binary: it has to
//! be a static executable, because a world holds no dynamic loader. It is looked for in one order —
//! the variable, then beside this binary, then where `scripts/build-probe.sh` puts it — and a
//! dynamic one is passed over, since the workspace build leaves one beside a development binary.

use std::path::{Path, PathBuf};

use crate::CliError;

/// The variable naming the probe executable, ahead of anywhere it is looked for.
pub(crate) const PROBE_VARIABLE: &str = "BOHR_PROBE";

/// The file the probe is executed from: a copy at `/tmp/bohr-probe-` and its digest's first 16
/// hex digits, made where it is not there yet.
///
/// A path of one length on every host, because the kernel copies the executed path onto the
/// guest's stack, and a stack laid out by where a host keeps its files is a run that replays only
/// on that host.
pub(crate) fn program() -> Result<String, CliError> {
    // Once per process: a campaign builds every run's guests from here, and the probe does not
    // move while one runs.
    static PLACED: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    PLACED.get_or_init(|| place().map_err(|e| e.to_string())).clone().map_err(CliError::Usage)
}

fn place() -> Result<String, CliError> {
    let found = found()?;
    let bytes = std::fs::read(&found)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {found}: {e}")))?;
    let digest = bohr_types::Digest::of(&bytes).to_string();
    let hex = digest.trim_start_matches("blake3:");
    let fixed = format!("/tmp/bohr-probe-{}", &hex[..16.min(hex.len())]);
    if std::fs::read(&fixed).is_ok_and(|held| held == bytes) {
        return Ok(fixed);
    }
    // Written beside and renamed into place, so two runs making the copy at once never execute a
    // half-written file.
    let part = format!("{fixed}.{}", std::process::id());
    let cannot = |e: std::io::Error| {
        CliError::Usage(format!("BOHR-1002: cannot place the probe at {fixed}: {e}"))
    };
    std::fs::write(&part, &bytes).map_err(cannot)?;
    let executable: std::fs::Permissions = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&part, executable).map_err(cannot)?;
    std::fs::rename(&part, &fixed).map_err(cannot)?;
    Ok(fixed)
}

/// The probe this host has, by the order the module names.
fn found() -> Result<String, CliError> {
    if let Ok(named) = std::env::var(PROBE_VARIABLE) {
        return match static_executable(Path::new(&named)) {
            Ok(()) => Ok(named),
            Err(why) => Err(CliError::Usage(format!("BOHR-1002: {PROBE_VARIABLE}={named}: {why}"))),
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
    Err(CliError::Usage(format!(
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
