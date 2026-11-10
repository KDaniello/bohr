//! The framed event log.
//!
//! Each record is `u32 length | CBOR | u32 length`, so boundaries are found from either end and a
//! log from a supervisor that died mid-run is still readable.

use std::io;

use bohr_types::Digest;

use crate::{TraceError, event::Event};

/// Bytes of framing around each record.
const FRAME_OVERHEAD: usize = 8;

/// Largest record this reader will allocate for, guarding against a corrupt length field.
const MAX_RECORD_BYTES: u32 = 64 * 1024 * 1024;

/// Writes events to a sink while maintaining the running trace digest.
#[derive(Debug)]
pub struct TraceWriter<W: io::Write> {
    inner: W,
    hasher: blake3::Hasher,
    count: u64,
    /// Reused across records, since a run records millions.
    scratch: Vec<u8>,
    /// Whether the log is kept or only digested.
    keep: bool,
    stats: TraceStats,
}

/// What a writer learned from the events it saw, so nothing has to read the log back to know it.
#[derive(Clone, Debug, Default)]
pub struct TraceStats {
    /// Syscall events.
    pub syscalls: u64,
    /// Syscall events by number, for the numbers below 512.
    pub by_nr: Vec<u64>,
    /// Nodes and tasks that recorded anything.
    pub nodes: std::collections::BTreeSet<bohr_types::NodeId>,
    pub tasks: std::collections::BTreeSet<bohr_types::TaskId>,
    /// The last event recorded.
    pub last: Option<bohr_types::EventId>,
    /// The most recent diagnostics, as `(code, message)`, oldest first.
    pub diagnostics: std::collections::VecDeque<(String, String)>,
}

/// Diagnostics a writer remembers, the most recent ones.
const KEPT_DIAGNOSTICS: usize = 8;

impl<W: io::Write> TraceWriter<W> {
    /// Wraps a sink that keeps the log.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            count: 0,
            scratch: Vec::new(),
            keep: true,
            stats: TraceStats::default(),
        }
    }

    /// Wraps a sink, digesting every event and encoding none: the log of a run nobody will read.
    pub fn digest_only(inner: W) -> Self {
        Self { keep: false, ..Self::new(inner) }
    }

    /// What the events recorded so far amount to.
    #[must_use]
    pub const fn stats(&self) -> &TraceStats {
        &self.stats
    }

    /// Appends one event.
    pub fn record(&mut self, event: &Event) -> Result<(), TraceError> {
        if self.keep {
            self.scratch.clear();
            ciborium::into_writer(event, &mut self.scratch)
                .map_err(|e| TraceError::Encode(e.to_string()))?;
            let len = u32::try_from(self.scratch.len()).map_err(|_| TraceError::RecordTooLarge)?;
            if len > MAX_RECORD_BYTES {
                return Err(TraceError::RecordTooLarge);
            }
            self.inner.write_all(&len.to_le_bytes())?;
            self.inner.write_all(&self.scratch)?;
            self.inner.write_all(&len.to_le_bytes())?;
        }

        let stats = &mut self.stats;
        stats.nodes.insert(event.node);
        stats.tasks.insert(event.task);
        stats.last = Some(event.id);
        match &event.kind {
            crate::EventKind::Syscall { nr, .. } => {
                stats.syscalls += 1;
                let at = *nr as usize;
                if at < 512 {
                    if stats.by_nr.len() <= at {
                        stats.by_nr.resize(512, 0);
                    }
                    stats.by_nr[at] += 1;
                }
            }
            crate::EventKind::Diagnostic { code, message } => {
                // Distinct ones, so a repeated line does not push out the rest.
                let said = (code.clone(), message.clone());
                stats.diagnostics.retain(|kept| *kept != said);
                if stats.diagnostics.len() == KEPT_DIAGNOSTICS {
                    stats.diagnostics.pop_front();
                }
                stats.diagnostics.push_back(said);
            }
            _ => {}
        }

        event.absorb(&mut self.hasher);
        #[cfg(feature = "chaos-wall-clock-digest")]
        crate::chaos::absorb_elapsed(&mut self.hasher);
        self.count += 1;
        Ok(())
    }

    /// Digest of everything recorded so far.
    #[must_use]
    pub fn digest(&self) -> Digest {
        Digest(*self.hasher.finalize().as_bytes())
    }

    /// Events recorded so far.
    #[inline]
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Flushes and yields the sink, the digest and the event count.
    pub fn finish(mut self) -> Result<(W, Digest, u64), TraceError> {
        self.inner.flush()?;
        let digest = self.digest();
        Ok((self.inner, digest, self.count))
    }

    /// Yields the sink and what the writer learned, without flushing it.
    pub fn into_parts(self) -> (W, TraceStats) {
        (self.inner, self.stats)
    }
}

/// Digest of a complete event sequence.
#[must_use]
pub fn digest_of(events: &[Event]) -> Digest {
    let mut hasher = blake3::Hasher::new();
    for e in events {
        e.absorb(&mut hasher);
    }
    Digest(*hasher.finalize().as_bytes())
}

/// Reads a complete log, rejecting any damage.
pub fn read_events(bytes: &[u8]) -> Result<Vec<Event>, TraceError> {
    let (events, err) = read_events_lossy(bytes);
    match err {
        Some(e) => Err(e),
        None => Ok(events),
    }
}

/// Reads as much of a log as is intact, reporting where it stopped.
///
/// A truncated log is the normal result of a crash, so the intact prefix is kept.
#[must_use]
pub fn read_events_lossy(bytes: &[u8]) -> (Vec<Event>, Option<TraceError>) {
    let mut events = Vec::new();
    let mut pos = 0usize;

    while pos < bytes.len() {
        if bytes.len() - pos < FRAME_OVERHEAD {
            return (events, Some(TraceError::TruncatedAt { offset: pos as u64 }));
        }
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        if len > MAX_RECORD_BYTES {
            return (events, Some(TraceError::RecordTooLarge));
        }
        let len = len as usize;
        let end = match pos.checked_add(FRAME_OVERHEAD).and_then(|v| v.checked_add(len)) {
            Some(e) if e <= bytes.len() => e,
            _ => return (events, Some(TraceError::TruncatedAt { offset: pos as u64 })),
        };

        let payload = &bytes[pos + 4..pos + 4 + len];
        let trailer =
            u32::from_le_bytes([bytes[end - 4], bytes[end - 3], bytes[end - 2], bytes[end - 1]]);
        if trailer as usize != len {
            return (events, Some(TraceError::FrameMismatch { offset: pos as u64 }));
        }

        match ciborium::from_reader::<Event, _>(payload) {
            Ok(e) => events.push(e),
            Err(e) => return (events, Some(TraceError::Decode(e.to_string()))),
        }
        pos = end;
    }

    (events, None)
}

#[cfg(test)]
mod tests {
    use bohr_types::{EventId, NodeId, TaskId, Tick};

    use super::*;
    use crate::event::EventKind;

    fn sample(id: u64) -> Event {
        Event {
            id: EventId(id),
            tick: Tick(id * 1000),
            node: NodeId(0),
            task: TaskId(1),
            kind: EventKind::ClockRead { presented: Tick(id * 1000) },
        }
    }

    fn log_of(n: u64) -> (Vec<u8>, Digest, u64) {
        let mut w = TraceWriter::new(Vec::new());
        for i in 0..n {
            w.record(&sample(i)).expect("record");
        }
        w.finish().expect("finish")
    }

    #[test]
    fn round_trips_and_the_digest_matches_a_direct_computation() {
        let (bytes, digest, count) = log_of(64);
        assert_eq!(count, 64);
        let back = read_events(&bytes).expect("intact log");
        assert_eq!(back.len(), 64);
        assert_eq!(digest_of(&back), digest);
    }

    #[test]
    fn truncation_at_every_offset_yields_a_prefix_or_an_error_and_never_garbage() {
        let (bytes, _, _) = log_of(16);
        let mut clean_cuts = 0;

        for cut in 0..=bytes.len() {
            let (events, err) = read_events_lossy(&bytes[..cut]);

            // Recovered events are always a prefix of what was written, in order.
            for (i, e) in events.iter().enumerate() {
                assert_eq!(e.id, EventId(i as u64), "cut {cut} recovered events out of order");
            }
            assert!(events.len() <= 16, "cut {cut} produced more events than were written");

            // A cut landing exactly on a record boundary is a shorter, valid log.
            match err {
                None => {
                    clean_cuts += 1;
                    assert_eq!(
                        read_events(&bytes[..cut]).expect("a clean cut must read strictly").len(),
                        events.len()
                    );
                }
                Some(_) => assert!(read_events(&bytes[..cut]).is_err()),
            }
        }

        // 17 boundaries: the empty log plus one after each of the 16 records.
        assert_eq!(clean_cuts, 17, "record boundaries should be the only clean cuts");
    }

    #[test]
    fn a_damaged_frame_is_reported_rather_than_guessed_at() {
        let (mut bytes, _, _) = log_of(4);
        let first_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        // Corrupt only the trailing copy of the first record's length.
        bytes[4 + first_len] ^= 0xFF;
        let (events, err) = read_events_lossy(&bytes);
        assert!(events.is_empty());
        assert!(matches!(err, Some(TraceError::FrameMismatch { offset: 0 })));
    }

    #[test]
    fn an_absurd_length_field_does_not_cause_a_large_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        let (events, err) = read_events_lossy(&bytes);
        assert!(events.is_empty());
        assert!(matches!(err, Some(TraceError::RecordTooLarge)));
    }
}
