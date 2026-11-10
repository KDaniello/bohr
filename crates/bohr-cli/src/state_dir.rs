//! Reading a prepared state off the host, and a scenario with the state it names.
//!
//! The world crate does no I/O: it parses a scenario and takes the files a state holds. This reads
//! them, refuses what a guest could not be shown, and checks the pin.

use std::{collections::BTreeMap, path::Path};

use bohr_world::{ConfigError, LoadedConfig, StateError, StateImage};

/// The largest file a prepared state may hold: every file is held in supervisor memory.
pub(crate) const MAX_STATE_FILE_BYTES: u64 = 1 << 26;

fn refused(message: String) -> StateError {
    StateError::Refused(message)
}

/// Reads a directory as the filesystem a node will start from.
///
/// Every refusal names the path.
///
/// # Errors
///
/// Refuses a symlink, anything that is not a regular file, a path that is not UTF-8, and a file
/// larger than `largest_file`.
pub(crate) fn read(root: &Path, largest_file: u64) -> Result<StateImage, StateError> {
    let mut files = BTreeMap::new();
    walk(root, root, &mut files, largest_file)?;
    Ok(StateImage::from_files(files))
}

/// A scenario's text, parsed, with the prepared state it names read and checked against its pin.
///
/// # Errors
///
/// The text does not parse, the state does not read, or it is not the state the pin names.
pub(crate) fn load(text: &str) -> Result<LoadedConfig, ConfigError> {
    let mut loaded = bohr_world::config::parse(text)?;
    if let Some(source) = &loaded.state_source {
        let image = read(Path::new(&source.dir), MAX_STATE_FILE_BYTES)
            .map_err(|e| ConfigError::Refused(e.to_string()))?;
        // The pin names the exact state; it is checked before anything runs.
        if let Some(pinned) = source.digest.as_deref() {
            let actual = image.digest().to_hex();
            if pinned.trim_start_matches("blake3:") != actual {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: state.digest names {pinned}, and {} holds {actual}",
                    source.dir
                )));
            }
        }
        loaded.state = Some(image);
    }
    Ok(loaded)
}

/// Walks a directory, adding every file under the path a guest will open it at.
fn walk(
    root: &Path,
    at: &Path,
    into: &mut BTreeMap<String, Vec<u8>>,
    largest_file: u64,
) -> Result<(), StateError> {
    let entries = std::fs::read_dir(at)
        .map_err(|e| refused(format!("BOHR-1002: cannot read {}: {e}", at.display())))?;

    // Sorted, since host directory order varies.
    let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| refused(format!("BOHR-1002: cannot read {}: {e}", path.display())))?;

        if meta.is_dir() {
            walk(root, &path, into, largest_file)?;
            continue;
        }
        // Symlinks are refused.
        if meta.is_symlink() {
            return Err(refused(format!(
                "BOHR-1002: {} is a symbolic link, which this world does not present",
                path.display()
            )));
        }
        if !meta.is_file() {
            return Err(refused(format!("BOHR-1002: {} is not a regular file", path.display())));
        }
        if meta.len() > largest_file {
            return Err(refused(format!(
                "BOHR-1002: {} is {} bytes and the largest a guest may be shown is {largest_file}",
                path.display(),
                meta.len()
            )));
        }

        let guest_path = guest_path(root, &path)?;
        let content = std::fs::read(&path)
            .map_err(|e| refused(format!("BOHR-1002: cannot read {}: {e}", path.display())))?;
        into.insert(guest_path, content);
    }
    Ok(())
}

/// The path a guest opens a file at: its place under the root, with forward slashes.
fn guest_path(root: &Path, path: &Path) -> Result<String, StateError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        refused(format!("BOHR-1002: {} is not under the state directory", path.display()))
    })?;
    let text = relative
        .to_str()
        .ok_or_else(|| refused(format!("BOHR-1002: {} is not a UTF-8 path", relative.display())))?;
    // Absolute with forward slashes, on every host.
    Ok(format!("/{}", text.replace('\\', "/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a directory of this test's own");
        }
        std::fs::write(path, bytes).expect("the file can be written");
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bohr-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory of this test's own");
        dir
    }

    #[test]
    fn a_directory_becomes_the_paths_a_guest_will_open() {
        let dir = scratch("paths");
        write(&dir, "store.db", b"records");
        write(&dir, "conf/node.toml", b"id = 1");

        let image = read(&dir, 1 << 20).expect("it reads");
        assert_eq!(image.size(), (2, 13));
        let manifest = image.manifest();
        assert!(manifest.contains("\"/conf/node.toml\""), "{manifest}");
        assert!(manifest.contains("\"/store.db\""), "{manifest}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The digest depends only on content.
    #[test]
    fn the_same_content_has_the_same_digest_wherever_it_was_read() {
        let here = scratch("digest-a");
        let there = scratch("digest-b");
        for dir in [&here, &there] {
            write(dir, "store.db", b"records");
            write(dir, "conf/node.toml", b"id = 1");
        }
        let a = read(&here, 1 << 20).expect("it reads");
        let b = read(&there, 1 << 20).expect("it reads");
        assert_eq!(a.digest(), b.digest());
        let _ = std::fs::remove_dir_all(&here);
        let _ = std::fs::remove_dir_all(&there);
    }

    #[test]
    fn a_file_too_large_to_present_is_refused_by_name() {
        let dir = scratch("large");
        write(&dir, "store.db", &[0u8; 64]);

        let refused = read(&dir, 32).expect_err("it is too large");
        let said = format!("{refused}");
        assert!(said.contains("store.db"), "{said}");
        assert!(said.contains("BOHR-1002"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pin that names another state is refused when the scenario is loaded.
    #[test]
    fn a_state_that_is_not_the_pinned_one_is_refused() {
        let dir = scratch("pinned");
        write(&dir, "store.db", b"records");
        let text = format!("[state]\ndir = '{}'\ndigest = 'blake3:00'\n", dir.display());
        let refused = load(&text).expect_err("another state");
        assert!(refused.to_string().contains("state.digest"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
