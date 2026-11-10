//! A bundle section written to a file as it is produced, so no copy of it is ever held whole.
//!
//! A campaign saves a finding by running it again with its log kept. Four Go validators write
//! about 400 000 events per virtual second, and a log held in memory, copied into the bundle and
//! compressed there, took a worker past 3 GB. A spool compresses each event into a file as it is
//! recorded and hashes the bytes on the way, so the section costs the compressor's window.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use bohr_types::Digest;

use crate::bundle::COMPRESS_LEVEL;

/// A section being written to a file, compressed, with the hash and length of what went in.
pub struct Spool {
    path: PathBuf,
    encoder: zstd::stream::write::Encoder<'static, BufWriter<File>>,
    hasher: blake3::Hasher,
    original: u64,
}

impl std::fmt::Debug for Spool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spool").field("path", &self.path).field("original", &self.original).finish()
    }
}

impl Spool {
    /// A spool writing to `path`, which it creates or truncates.
    ///
    /// # Errors
    ///
    /// The file cannot be created, or the compressor cannot start.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = BufWriter::new(File::create(path)?);
        Ok(Self {
            path: path.to_owned(),
            encoder: zstd::stream::write::Encoder::new(file, COMPRESS_LEVEL)?,
            hasher: blake3::Hasher::new(),
            original: 0,
        })
    }

    /// Ends the section and says what the file holds.
    ///
    /// # Errors
    ///
    /// The compressor's last block or the file cannot be written.
    pub fn finish(self) -> io::Result<Spooled> {
        let mut file = self.encoder.finish()?;
        file.flush()?;
        let stored = file.get_ref().metadata()?.len();
        Ok(Spooled {
            path: self.path,
            stored,
            original: self.original,
            hash: Digest(*self.hasher.finalize().as_bytes()),
        })
    }
}

impl Write for Spool {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.encoder.write_all(buf)?;
        self.hasher.update(buf);
        self.original += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.encoder.flush()
    }
}

/// A finished spool: a compressed section in a file, its stored and original lengths, and the
/// hash of the original bytes. The file is removed when this is dropped.
#[derive(Debug)]
pub struct Spooled {
    path: PathBuf,
    /// Bytes in the file.
    pub stored: u64,
    /// Bytes the section unpacks to.
    pub original: u64,
    /// blake3 of the bytes the section unpacks to.
    pub hash: Digest,
}

impl Spooled {
    /// The file the compressed section is in.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The bytes the section unpacks to, read back whole.
    ///
    /// For a reader that needs the log in memory anyway, such as a run printed after it ends.
    ///
    /// # Errors
    ///
    /// The file cannot be read or does not unpack to its recorded length.
    pub fn unpack(&self) -> io::Result<Vec<u8>> {
        use std::io::Read as _;
        let mut out = Vec::new();
        zstd::stream::read::Decoder::new(File::open(&self.path)?)?.read_to_end(&mut out)?;
        if out.len() as u64 != self.original {
            return Err(io::Error::other(format!(
                "unpacked to {} bytes, {} recorded",
                out.len(),
                self.original
            )));
        }
        Ok(out)
    }
}

impl Spooled {
    /// The events `keep` accepts, read from the file a frame at a time, so a log of any size
    /// costs only what is kept.
    ///
    /// # Errors
    ///
    /// The file cannot be opened or its compression cannot start.
    pub fn events_where(
        &self,
        keep: impl Fn(&crate::Event) -> bool,
    ) -> io::Result<(Vec<crate::Event>, Option<crate::TraceError>)> {
        let decoder = zstd::stream::read::Decoder::new(File::open(&self.path)?)?;
        Ok(crate::read_events_where(decoder, keep))
    }
}

impl Drop for Spooled {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spool_holds_what_was_written_and_hashes_it() {
        let path = std::env::temp_dir().join("bohr-spool-test.zst");
        let mut spool = Spool::create(&path).expect("a spool");
        let bytes: Vec<u8> = (0..200_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        for chunk in bytes.chunks(997) {
            spool.write_all(chunk).expect("written");
        }
        let spooled = spool.finish().expect("finished");
        assert_eq!(spooled.original, bytes.len() as u64);
        assert_eq!(spooled.hash, Digest::of(&bytes));
        assert!(spooled.stored < spooled.original, "compressed");
        assert_eq!(spooled.unpack().expect("unpacked"), bytes);
        drop(spooled);
        assert!(!path.exists(), "the file goes with the spool");
    }
}
