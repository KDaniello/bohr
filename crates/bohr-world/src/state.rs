//! The filesystem a node starts from.
//!
//! A real client cannot sync for days before every run, so a run can start from a prepared state.
//!
//! The state is a host directory named in the configuration and pinned by digest. The bundle
//! records which state, never its content, and a replay against another is refused.
//!
//! Every file is held in supervisor memory, which bounds the state's size.

use std::{collections::BTreeMap, path::Path};

use bohr_types::Digest;

/// Why a prepared state was refused.
///
/// One variant; the message names the path.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Named in the message, always with the path that caused it.
    #[error("{0}")]
    Refused(String),
}

/// Files a run starts with, and what they add up to.
#[derive(Clone, Debug)]
pub struct StateImage {
    /// Contents by the path a guest opens them at, in sorted order.
    ///
    /// Sorted, since inode numbers follow insertion order.
    files: BTreeMap<String, Vec<u8>>,
    /// What this image is, independently of the machine it was built on.
    digest: Digest,
}

impl StateImage {
    /// Reads a directory as the filesystem a node will start from.
    ///
    /// Every refusal names the path.
    ///
    /// # Errors
    ///
    /// Refuses a symlink, anything that is not a regular file, a path that is not UTF-8, and a
    /// file larger than the world can hold.
    pub fn from_dir(root: &Path, largest_file: u64) -> Result<Self, StateError> {
        let mut files = BTreeMap::new();
        walk(root, root, &mut files, largest_file)?;
        let digest = digest_of(&files);
        Ok(Self { files, digest })
    }

    /// The same image with the instructions that take a number from the processor replaced.
    ///
    /// On a copy; host files are never touched (`ARCHITECTURE.md` §5.8). The digest changes with
    /// the contents.
    ///
    /// The program itself is not in the image and is only reported. Its libraries are, such as
    /// `libstdc++` with five sites on the `std::random_device` path.
    #[must_use]
    pub fn without_hardware_randomness(self) -> (Self, Vec<(String, usize)>) {
        let mut files = BTreeMap::new();
        let mut rewritten = Vec::new();
        for (path, content) in self.files {
            match bohr_backend::code::entropy::without_hardware_randomness(&content) {
                Some((patched, sites)) => {
                    rewritten.push((path.clone(), sites.len()));
                    files.insert(path, patched);
                }
                None => {
                    files.insert(path, content);
                }
            }
        }
        let digest = digest_of(&files);
        (Self { files, digest }, rewritten)
    }

    /// The same image with the instruction that asks the processor what it is made to fault.
    ///
    /// On a copy; the digest changes with the contents. The program is rewritten separately by the
    /// caller, and the fault is recognised by its byte.
    #[must_use]
    pub fn with_processor_questions_trapped(self) -> (Self, Vec<(String, usize)>) {
        let mut files = BTreeMap::new();
        let mut rewritten = Vec::new();
        for (path, content) in self.files {
            match bohr_backend::code::cpuid::trapped(&content) {
                Some((patched, sites)) => {
                    rewritten.push((path.clone(), sites.len()));
                    files.insert(path, patched);
                }
                None => {
                    files.insert(path, content);
                }
            }
        }
        let digest = digest_of(&files);
        (Self { files, digest }, rewritten)
    }

    /// What this image is: `blake3` over `(path, length, content digest)` in sorted order.
    ///
    /// Content and names only, so two machines holding the same state agree.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }

    /// Every file, by the path a guest opens it at.
    pub fn files(&self) -> impl Iterator<Item = (&String, &Vec<u8>)> {
        self.files.iter()
    }

    /// How many files, and how many bytes in total.
    #[must_use]
    pub fn size(&self) -> (usize, u64) {
        (self.files.len(), self.files.values().map(|c| c.len() as u64).sum())
    }

    /// The per-file list a bundle records, as JSON.
    #[must_use]
    pub fn manifest(&self) -> String {
        let mut out = String::from("[");
        for (i, (path, content)) in self.files.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n  {{\"path\": {:?}, \"len\": {}, \"digest\": \"{}\"}}",
                path,
                content.len(),
                Digest::of(content).to_hex()
            ));
        }
        out.push_str("\n]\n");
        out
    }
}

/// Walks a directory, adding every file under the path a guest will open it at.
fn walk(
    root: &Path,
    at: &Path,
    into: &mut BTreeMap<String, Vec<u8>>,
    largest_file: u64,
) -> Result<(), StateError> {
    let entries = std::fs::read_dir(at).map_err(|e| {
        StateError::Refused(format!("BOHR-1002: cannot read {}: {e}", at.display()))
    })?;

    // Sorted, since host directory order varies.
    let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let meta = std::fs::symlink_metadata(&path).map_err(|e| {
            StateError::Refused(format!("BOHR-1002: cannot read {}: {e}", path.display()))
        })?;

        if meta.is_dir() {
            walk(root, &path, into, largest_file)?;
            continue;
        }
        // Symlinks are refused.
        if meta.is_symlink() {
            return Err(StateError::Refused(format!(
                "BOHR-1002: {} is a symbolic link, which this world does not present",
                path.display()
            )));
        }
        if !meta.is_file() {
            return Err(StateError::Refused(format!(
                "BOHR-1002: {} is not a regular file",
                path.display()
            )));
        }
        if meta.len() > largest_file {
            return Err(StateError::Refused(format!(
                "BOHR-1002: {} is {} bytes and the largest a guest may be shown is {largest_file}",
                path.display(),
                meta.len()
            )));
        }

        let guest_path = guest_path(root, &path)?;
        let content = std::fs::read(&path).map_err(|e| {
            StateError::Refused(format!("BOHR-1002: cannot read {}: {e}", path.display()))
        })?;
        into.insert(guest_path, content);
    }
    Ok(())
}

/// The path a guest opens a file at: its place under the root, with forward slashes.
fn guest_path(root: &Path, path: &Path) -> Result<String, StateError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        StateError::Refused(format!(
            "BOHR-1002: {} is not under the state directory",
            path.display()
        ))
    })?;
    let text = relative.to_str().ok_or_else(|| {
        StateError::Refused(format!("BOHR-1002: {} is not a UTF-8 path", relative.display()))
    })?;
    // Absolute with forward slashes, on every host.
    Ok(format!("/{}", text.replace('\\', "/")))
}

/// What an image is, independently of the machine it was read on.
fn digest_of(files: &BTreeMap<String, Vec<u8>>) -> Digest {
    let mut canonical = Vec::new();
    for (path, content) in files {
        canonical.extend_from_slice(path.as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(&(content.len() as u64).to_le_bytes());
        canonical.extend_from_slice(Digest::of(content).as_bytes());
    }
    Digest::of(&canonical)
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
        let dir = std::env::temp_dir().join(format!("bohr-state-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory of this test's own");
        dir
    }

    #[test]
    fn a_directory_becomes_the_paths_a_guest_will_open() {
        let dir = scratch("paths");
        write(&dir, "store.db", b"records");
        write(&dir, "conf/node.toml", b"id = 1");

        let image = StateImage::from_dir(&dir, 1 << 20).expect("it reads");
        let paths: Vec<&String> = image.files().map(|(p, _)| p).collect();
        assert_eq!(paths, vec!["/conf/node.toml", "/store.db"]);
        assert_eq!(image.size(), (2, 13));
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

        let a = StateImage::from_dir(&here, 1 << 20).expect("it reads");
        let b = StateImage::from_dir(&there, 1 << 20).expect("it reads");
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn a_changed_byte_is_a_different_state() {
        let dir = scratch("changed");
        write(&dir, "store.db", b"records");
        let before = StateImage::from_dir(&dir, 1 << 20).expect("it reads").digest();

        write(&dir, "store.db", b"recordS");
        let after = StateImage::from_dir(&dir, 1 << 20).expect("it reads").digest();
        assert_ne!(before, after);
    }

    #[test]
    fn a_file_too_large_to_present_is_refused_by_name() {
        let dir = scratch("large");
        write(&dir, "store.db", &[0u8; 64]);

        let refused = StateImage::from_dir(&dir, 32).expect_err("it is too large");
        let said = format!("{refused}");
        assert!(said.contains("store.db"), "{said}");
        assert!(said.contains("BOHR-1002"), "{said}");
    }
}
