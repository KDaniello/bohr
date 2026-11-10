//! The directory the programs bohr supplies are executed from: the probe and a trapped copy.
//!
//! The shared temporary directory is writable by every user, so a predictable name there can be
//! planted before bohr writes it, and bohr would execute the planted file. This directory is the
//! user's alone, and a file in it is named by its content, so a name found there holds the bytes it
//! names.

#[cfg(unix)]
use std::{
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[cfg(unix)]
use crate::CliError;

/// The user's directory under the temporary directory, created with mode 0700 where it is absent.
///
/// # Errors
///
/// `BOHR-1002` when it exists and is a symlink, not a directory, another user's, or open to others.
#[cfg(unix)]
pub(crate) fn dir() -> Result<PathBuf, CliError> {
    // The owner of `/proc/self` is this process's user, which spares a C call.
    let uid = std::fs::metadata("/proc/self")
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot learn this user's id: {e}")))?
        .uid();
    let dir = std::env::temp_dir().join(format!("bohr-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => return Ok(dir),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(CliError::usage(format!(
                "BOHR-1002: cannot create {}: {e}",
                dir.display()
            )));
        }
    }
    // Not followed: a symlink there is someone else's way in.
    let meta = std::fs::symlink_metadata(&dir)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {}: {e}", dir.display())))?;
    let why = if !meta.file_type().is_dir() {
        Some("is not a directory")
    } else if meta.uid() != uid {
        Some("belongs to another user")
    } else if meta.permissions().mode() & 0o077 != 0 {
        Some("is open to other users")
    } else {
        None
    };
    match why {
        Some(why) => Err(CliError::usage(format!(
            "BOHR-1002: {} {why}, so nothing is executed from it; remove it",
            dir.display()
        ))),
        None => Ok(dir),
    }
}

/// Places `bytes` as an executable named `<stem>-<digest>` in [`dir`] and names its path.
///
/// A file of that name is the same bytes, since only this user writes there; a new one is written
/// to a fresh name and renamed, so a concurrent run never executes half a file.
///
/// # Errors
///
/// `BOHR-1002` when the directory is refused or the file cannot be written.
#[cfg(unix)]
pub(crate) fn place(stem: &str, bytes: &[u8]) -> Result<String, CliError> {
    let digest = bohr_types::Digest::of(bytes).to_string();
    let hex = digest.trim_start_matches("blake3:");
    let path = dir()?.join(format!("{stem}-{}", &hex[..16.min(hex.len())]));
    let cannot = |e: std::io::Error| {
        CliError::usage(format!("BOHR-1002: cannot place {}: {e}", path.display()))
    };
    if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64) {
        return Ok(path.to_string_lossy().into_owned());
    }
    let part = path.with_extension(format!("part-{}", std::process::id()));
    let _ = std::fs::remove_file(&part);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&part)
        .map_err(cannot)?;
    file.write_all(bytes).map_err(cannot)?;
    file.sync_all().map_err(cannot)?;
    drop(file);
    std::fs::rename(&part, &path).map_err(cannot)?;
    Ok(path.to_string_lossy().into_owned())
}

/// The variable naming files executed where a scenario starts a program by another name, one
/// `<name>=<file>` a line: a hunt's `--binary` in place of the pack's own, through every worker.
pub(crate) const EXECUTE_VARIABLE: &str = "BOHR_EXECUTE";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
/// Files executed in place of programs, by the name the scenario starts each by.
static SUBSTITUTES: std::sync::Mutex<std::collections::BTreeMap<String, String>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
/// Executes `file` wherever a guest is started as `name`; the guest still sees `name`.
pub(crate) fn substitute(name: &str, file: &str) {
    if let Ok(mut held) = SUBSTITUTES.lock() {
        held.insert(name.to_owned(), file.to_owned());
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
/// The file executed for a guest started as `name`, where one stands in for it.
pub(crate) fn substitute_for(name: &str) -> Option<String> {
    if let Ok(named) = std::env::var(EXECUTE_VARIABLE)
        && let Some(file) = named.lines().find_map(|line| {
            line.split_once('=').filter(|(from, _)| *from == name).map(|(_, file)| file)
        })
    {
        return Some(file.to_owned());
    }
    SUBSTITUTES.lock().ok().and_then(|held| held.get(name).cloned())
}
