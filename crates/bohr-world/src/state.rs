//! The filesystem a node starts from.
//!
//! A real client cannot sync for days before every run, so a run can start from a prepared state.
//!
//! The state is a host directory named in the configuration and pinned by digest. The bundle
//! records which state, never its content, and a replay against another is refused. The directory
//! is read by the command line; this crate takes the files it read.
//!
//! Every file is held in supervisor memory, which bounds the state's size. It is held once: every
//! node of every run starts from blocks it shares with the image, and copies a block only when it
//! writes into it.

use std::collections::BTreeMap;

use bohr_types::Digest;

use crate::bytes::FileBytes;

/// Why a prepared state was refused.
///
/// One variant; the message names the path.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Named in the message, always with the path that caused it.
    #[error("{0}")]
    Refused(String),
}

/// One file of an image: its bytes, in blocks a node's copy shares, and their digest.
#[derive(Clone, Debug)]
struct Held {
    bytes: FileBytes,
    digest: Digest,
}

impl Held {
    fn of(content: Vec<u8>) -> Self {
        let digest = Digest::of(&content);
        Self { bytes: FileBytes::from_vec(content), digest }
    }
}

/// Files a run starts with, and what they add up to.
///
/// A clone shares every file's blocks.
#[derive(Clone, Debug)]
pub struct StateImage {
    /// Contents by the path a guest opens them at, in sorted order.
    ///
    /// Sorted, since inode numbers follow insertion order.
    files: BTreeMap<String, Held>,
    /// What this image is, independently of the machine it was built on.
    digest: Digest,
}

impl StateImage {
    /// The image of these files, by the path a guest opens each at.
    #[must_use]
    pub fn from_files(read: BTreeMap<String, Vec<u8>>) -> Self {
        let files: BTreeMap<String, Held> =
            read.into_iter().map(|(path, content)| (path, Held::of(content))).collect();
        let digest = digest_of(&files);
        Self { files, digest }
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
        for (path, held) in self.files {
            let content = held.bytes.to_vec();
            match bohr_x86::entropy::without_hardware_randomness(&content) {
                Some((patched, sites)) => {
                    rewritten.push((path.clone(), sites.len()));
                    files.insert(path, Held::of(patched));
                }
                None => {
                    files.insert(path, held);
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
        for (path, held) in self.files {
            let content = held.bytes.to_vec();
            match bohr_x86::cpuid::trapped(&content) {
                Some((patched, sites)) => {
                    rewritten.push((path.clone(), sites.len()));
                    files.insert(path, Held::of(patched));
                }
                None => {
                    files.insert(path, held);
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

    /// Every file, by the path a guest opens it at, in the blocks a node's copy shares.
    pub(crate) fn files(&self) -> impl Iterator<Item = (&String, &FileBytes)> {
        self.files.iter().map(|(path, held)| (path, &held.bytes))
    }

    /// How many files, and how many bytes in total.
    #[must_use]
    pub fn size(&self) -> (usize, u64) {
        (self.files.len(), self.files.values().map(|held| held.bytes.len() as u64).sum())
    }

    /// The per-file list a bundle records, as JSON.
    #[must_use]
    pub fn manifest(&self) -> String {
        let mut out = String::from("[");
        for (i, (path, held)) in self.files.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n  {{\"path\": {:?}, \"len\": {}, \"digest\": \"{}\"}}",
                path,
                held.bytes.len(),
                held.digest.to_hex()
            ));
        }
        out.push_str("\n]\n");
        out
    }
}

/// What an image is, independently of the machine it was read on.
fn digest_of(files: &BTreeMap<String, Held>) -> Digest {
    let mut canonical = Vec::new();
    for (path, held) in files {
        canonical.extend_from_slice(path.as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(&(held.bytes.len() as u64).to_le_bytes());
        canonical.extend_from_slice(held.digest.as_bytes());
    }
    Digest::of(&canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(files: &[(&str, &[u8])]) -> StateImage {
        StateImage::from_files(files.iter().map(|(p, c)| ((*p).to_owned(), c.to_vec())).collect())
    }

    /// A node starts from the image's own blocks, and so does a clone of the image: the state is
    /// held once, however many nodes and runs start from it.
    #[test]
    fn a_node_shares_the_images_blocks_until_it_writes() {
        let image = image(&[("/store.db", &vec![7u8; 300_000])]);
        let (_, held) = image.files().next().expect("one file");
        let disk = crate::MinimalWorld::new().from_state(&image).disk_image();
        let inode = disk.names["/store.db"];
        assert!(disk.content[&inode].same(held), "the node copied the state");
        let cloned = image.clone();
        assert!(cloned.files().next().expect("one file").1.same(held), "a clone copied the state");
    }

    /// The digest is over names and contents alone, so a changed byte is another state.
    #[test]
    fn a_changed_byte_is_a_different_state() {
        let before = image(&[("/store.db", b"records"), ("/conf/node.toml", b"id = 1")]);
        let again = image(&[("/conf/node.toml", b"id = 1"), ("/store.db", b"records")]);
        let after = image(&[("/store.db", b"recordS"), ("/conf/node.toml", b"id = 1")]);
        assert_eq!(before.digest(), again.digest());
        assert_ne!(before.digest(), after.digest());
        assert_eq!(before.size(), (2, 13));
    }
}
