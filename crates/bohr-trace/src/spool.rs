//! A bundle section written to a file as it is produced, so no copy of it is ever held whole.
//!
//! A campaign saves a finding by running it again with its log kept. Four Go validators write
//! about 400 000 events per virtual second, and a log held in memory, copied into the bundle and
//! compressed there, took a worker past 3 GB. A spool compresses each event into a file as it is
//! recorded and hashes the bytes on the way, so the section costs the compressor's window.
//!
//! The compressing and hashing happen on a thread of the spool's own, fed batches through a
//! bounded channel, so the run loop that records the events only copies them (ARCHITECTURE §6.1).
//! A full channel makes the recorder wait: what is held stays at a few batches whatever the run.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::mpsc::{SyncSender, sync_channel},
    thread::JoinHandle,
};

use bohr_types::Digest;

use crate::bundle::COMPRESS_LEVEL;

/// Bytes gathered before a batch goes to the writer, so the channel carries few large batches.
const BATCH_BYTES: usize = 64 << 10;

/// Batches the writer may be behind before the recorder waits.
const BATCHES_AHEAD: usize = 4;

/// What the writer answers with once the channel closes: the file, and the hash of what went in.
type Written = (BufWriter<File>, Digest);

/// A section being written to a file, compressed, with the hash and length of what went in.
pub struct Spool {
    path: PathBuf,
    /// What is gathered for the next batch.
    batch: Vec<u8>,
    /// Where batches go; `None` once the spool is finished.
    batches: Option<SyncSender<Vec<u8>>>,
    /// The thread that compresses and hashes, which answers with the file and the hash.
    writer: Option<JoinHandle<io::Result<Written>>>,
    original: u64,
}

impl std::fmt::Debug for Spool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spool").field("path", &self.path).field("original", &self.original).finish()
    }
}

impl Spool {
    /// A spool writing to `path`, which it creates, readable only by its owner.
    ///
    /// A file or link already at `path` is refused, never written through: a spool lives in a
    /// shared temporary directory, where a name is anyone's to take first.
    ///
    /// # Errors
    ///
    /// The file exists or cannot be created, or the compressor cannot start.
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = BufWriter::new(options.open(path)?);
        let mut encoder = zstd::stream::write::Encoder::new(file, COMPRESS_LEVEL)?;
        let (batches, received) = sync_channel::<Vec<u8>>(BATCHES_AHEAD);
        let writer =
            std::thread::Builder::new().name("bohr-spool".to_owned()).spawn(move || {
                let mut hasher = blake3::Hasher::new();
                for batch in received {
                    encoder.write_all(&batch)?;
                    hasher.update(&batch);
                }
                let mut file = encoder.finish()?;
                file.flush()?;
                Ok((file, Digest(*hasher.finalize().as_bytes())))
            })?;
        Ok(Self {
            path: path.to_owned(),
            batch: Vec::with_capacity(BATCH_BYTES),
            batches: Some(batches),
            writer: Some(writer),
            original: 0,
        })
    }

    /// Ends the section and says what the file holds.
    ///
    /// # Errors
    ///
    /// The compressor's last block or the file cannot be written.
    pub fn finish(mut self) -> io::Result<Spooled> {
        self.send()?;
        let (file, hash) = self.join()?;
        let stored = file.get_ref().metadata()?.len();
        Ok(Spooled { path: std::mem::take(&mut self.path), stored, original: self.original, hash })
    }

    /// Hands the gathered bytes to the writer, waiting while it is `BATCHES_AHEAD` behind.
    fn send(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::replace(&mut self.batch, Vec::with_capacity(BATCH_BYTES));
        let sent = self.batches.as_ref().map(|batches| batches.send(batch));
        match sent {
            Some(Ok(())) => Ok(()),
            // The writer stopped, so it has an error to tell.
            _ => self.join().map(|_| ()).and(Err(io::Error::other("the spool's writer stopped"))),
        }
    }

    /// Closes the channel and waits for the writer's answer.
    fn join(&mut self) -> io::Result<Written> {
        self.batches = None;
        let writer = self.writer.take().ok_or_else(|| io::Error::other("the spool was ended"))?;
        writer.join().map_err(|_| io::Error::other("the spool's writer panicked"))?
    }
}

impl Drop for Spool {
    /// A spool dropped unfinished still lets its writer end before it goes.
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = self.join();
        }
    }
}

impl Write for Spool {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.batch.extend_from_slice(buf);
        self.original += buf.len() as u64;
        if self.batch.len() >= BATCH_BYTES {
            self.send()?;
        }
        Ok(buf.len())
    }

    /// Hands what is gathered to the writer; the file is written when the spool is finished.
    fn flush(&mut self) -> io::Result<()> {
        self.send()
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

    /// Bytes written in pieces of every size, across many batches, come back whole and in order,
    /// with the hash and length of what went in.
    #[test]
    fn what_the_writer_wrote_is_what_was_given() {
        let path = std::env::temp_dir().join(format!("bohr-spool-batches-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let given: Vec<u8> = (0..(5 * BATCH_BYTES as u32 + 77)).map(|i| (i % 251) as u8).collect();
        let mut spool = Spool::create(&path).expect("a spool");
        let mut at = 0;
        for size in [1usize, 7, 4096, BATCH_BYTES - 3, BATCH_BYTES + 9].iter().cycle() {
            let end = (at + size).min(given.len());
            spool.write_all(&given[at..end]).expect("written");
            at = end;
            if at == given.len() {
                break;
            }
        }
        let spooled = spool.finish().expect("finished");
        assert_eq!(spooled.original, given.len() as u64);
        assert_eq!(spooled.hash, Digest::of(&given));
        assert_eq!(spooled.unpack().expect("unpacked"), given);
    }

    /// A file already at a spool's name is refused and left as it was, never written through.
    #[test]
    fn a_spool_never_writes_through_a_name_another_took() {
        let path =
            std::env::temp_dir().join(format!("bohr-spool-taken-{}.zst", std::process::id()));
        std::fs::write(&path, b"planted").expect("planted");
        let refused = Spool::create(&path);
        let kept = std::fs::read(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        assert!(refused.is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists));
        assert_eq!(kept, b"planted");
    }

    #[test]
    fn a_spool_holds_what_was_written_and_hashes_it() {
        let path = std::env::temp_dir().join(format!("bohr-spool-test-{}.zst", std::process::id()));
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
