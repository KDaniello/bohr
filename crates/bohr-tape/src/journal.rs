//! The draw journal: what was decided, where on the tape, and by whom.
//!
//! Produced by a run, never read as input. The tape stays flat bytes for mutators; the journal
//! gives the reducer and the report the decision boundaries. See `docs/ARCHITECTURE.md` §5.3.
//!
//! Replay regenerates and compares it; a mismatch means the draw ABI changed.

use bohr_types::{EventId, TapeOffset};
use serde::{Deserialize, Serialize};

use crate::TapeError;

/// Size of one serialised [`DrawRecord`].
pub const DRAW_RECORD_BYTES: usize = 32;

/// Where a draw was made from.
///
/// The numeric values are part of the journal format and are never reused. See `docs/API.md` §2.5.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[repr(u16)]
pub enum DrawSite {
    /// Scheduler: which runnable task runs next.
    SchedPick = 1,
    /// PCT: initial priorities.
    SchedPriority = 2,
    /// PCT: where priority changes happen.
    SchedSwitchPoint = 3,
    /// Which waiter a synchronisation primitive releases.
    SyncWake = 4,

    /// Network: deliver, drop or duplicate.
    NetDeliver = 10,
    /// Network: delivery delay.
    NetDelay = 11,
    /// Network: reordering within a link.
    NetReorder = 12,
    /// Network: partition events.
    NetPartition = 13,
    /// Which socket of several sharing one port receives.
    NetSocket = 14,
    /// Network: whether a datagram is altered on the wire, and where.
    NetCorrupt = 15,

    /// Disk: operation stall.
    DiskLatency = 20,
    /// Disk: tear a write at a crash.
    DiskTear = 21,
    /// Disk: pre-`fsync` write reordering.
    DiskReorder = 22,
    /// Disk: `ENOSPC` / `EIO` injection.
    DiskError = 23,

    /// Node lifecycle: kill, freeze, restart.
    NodeFault = 30,
    /// Node clock: drift and step.
    ClockSkew = 31,
    /// Node memory: whether an allocation is refused.
    MemoryExhausted = 32,

    /// Guest randomness: `getrandom`, `/dev/urandom`, `AT_RANDOM`, the `RDRAND` trampoline.
    GuestRandom = 40,
    /// SDK `random_bytes`.
    SdkRandom = 41,
    /// SDK `fault_point`: fire or not.
    SdkFaultPoint = 42,
}

impl DrawSite {
    /// The name a report or a reduction shows for this site.
    ///
    /// Defined here, so every reader names a site the same way.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SchedPick => "which task runs next",
            Self::SchedPriority => "a task's initial priority",
            Self::SchedSwitchPoint => "where a priority changes",
            Self::SyncWake => "which waiter is released",
            Self::NetDeliver => "deliver, drop or duplicate",
            Self::NetDelay => "how long delivery takes",
            Self::NetReorder => "reordering within a link",
            Self::NetPartition => "a partition event",
            Self::NetSocket => "which socket of a shared port receives",
            Self::NetCorrupt => "whether and where a datagram is altered",
            Self::DiskLatency => "a disk stall",
            Self::DiskTear => "how a write tears",
            Self::DiskReorder => "reordering before a sync",
            Self::DiskError => "a disk error",
            Self::NodeFault => "whether a node dies",
            Self::ClockSkew => "a node's clock offset and drift",
            Self::MemoryExhausted => "whether memory ran out",
            Self::GuestRandom => "randomness the guest asked for",
            Self::SdkRandom => "randomness the SDK asked for",
            Self::SdkFaultPoint => "whether a fault point fires",
        }
    }

    /// The site a stored code names, where this build knows it.
    ///
    /// `None` for codes from a newer build, which do not invalidate the journal.
    #[must_use]
    pub const fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            1 => Self::SchedPick,
            2 => Self::SchedPriority,
            3 => Self::SchedSwitchPoint,
            4 => Self::SyncWake,
            10 => Self::NetDeliver,
            11 => Self::NetDelay,
            12 => Self::NetReorder,
            13 => Self::NetPartition,
            14 => Self::NetSocket,
            15 => Self::NetCorrupt,
            20 => Self::DiskLatency,
            21 => Self::DiskTear,
            22 => Self::DiskReorder,
            23 => Self::DiskError,
            30 => Self::NodeFault,
            31 => Self::ClockSkew,
            32 => Self::MemoryExhausted,
            40 => Self::GuestRandom,
            41 => Self::SdkRandom,
            42 => Self::SdkFaultPoint,
            _ => return None,
        })
    }
}

/// One decision: which bytes were consumed, when, by which site, and what came out.
///
/// `#[repr(C)]` with an explicit pad so the in-memory layout matches the on-disk one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(C)]
pub struct DrawRecord {
    /// Tape offset the draw started at.
    pub offset: u64,
    /// The event current when the draw happened.
    pub event: u64,
    /// The value produced. For a raw byte draw, the first up to eight bytes, little-endian.
    pub value: u64,
    /// Which site drew.
    pub site: u16,
    /// Reserved; must be zero.
    pub pad: u16,
    /// Bytes consumed. Zero for a degenerate choice, which consumes nothing by design.
    pub len: u32,
}

impl DrawRecord {
    /// Serialises to the fixed 32-byte little-endian form.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; DRAW_RECORD_BYTES] {
        let mut out = [0u8; DRAW_RECORD_BYTES];
        out[0..8].copy_from_slice(&self.offset.to_le_bytes());
        out[8..16].copy_from_slice(&self.event.to_le_bytes());
        out[16..24].copy_from_slice(&self.value.to_le_bytes());
        out[24..26].copy_from_slice(&self.site.to_le_bytes());
        out[26..28].copy_from_slice(&self.pad.to_le_bytes());
        out[28..32].copy_from_slice(&self.len.to_le_bytes());
        out
    }

    /// Parses the fixed 32-byte form.
    ///
    /// # Errors
    ///
    /// Refuses a non-zero reserved field, so file damage is reported as damage.
    pub fn from_bytes(b: &[u8; DRAW_RECORD_BYTES]) -> Result<Self, TapeError> {
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or_default());
        let pad = u16::from_le_bytes([b[26], b[27]]);
        if pad != 0 {
            return Err(TapeError::ReservedNotZero);
        }
        Ok(Self {
            offset: u64_at(0),
            event: u64_at(8),
            value: u64_at(16),
            site: u16::from_le_bytes([b[24], b[25]]),
            pad,
            len: u32::from_le_bytes(b[28..32].try_into().unwrap_or_default()),
        })
    }
}

/// Which of a run's draws a journal keeps; every draw is counted either way.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Keep {
    /// Every draw, as a bundle records them.
    #[default]
    All,
    /// The first `at_most` draws that read bytes inside the first `prefix` bytes of the tape:
    /// what a campaign remembers of a run, and all a run that keeps no log is asked for.
    Prefix {
        /// The tape's written prefix, in bytes.
        prefix: u64,
        /// The most records kept.
        at_most: usize,
    },
}

/// Append-only list of draws made during one run.
///
/// Equal by records alone; which odds draws fired is not stored.
#[derive(Clone, Default, Debug)]
pub struct Journal {
    records: Vec<DrawRecord>,
    /// Positions of the odds draws that fired, increasing.
    fired: Vec<usize>,
    keep: Keep,
    /// Draws made, kept or not.
    made: usize,
}

impl PartialEq for Journal {
    fn eq(&self, other: &Self) -> bool {
        self.records == other.records
    }
}

impl Eq for Journal {}

impl Journal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty journal that keeps only what `keep` names, and counts every draw.
    #[must_use]
    pub fn keeping(keep: Keep) -> Self {
        Self { keep, ..Self::default() }
    }

    /// A journal of records that were written down somewhere and read back.
    ///
    /// Takes records, which may carry site codes unknown to this build.
    #[must_use]
    pub fn from_records(records: Vec<DrawRecord>) -> Self {
        let made = records.len();
        Self { records, made, ..Self::default() }
    }

    /// Whether a draw of `len` bytes at `offset` is one this journal keeps.
    fn keeps(&self, offset: u64, len: u32) -> bool {
        match self.keep {
            Keep::All => true,
            Keep::Prefix { prefix, at_most } => {
                len > 0 && offset < prefix && self.records.len() < at_most
            }
        }
    }

    /// Records one draw.
    pub fn push(
        &mut self,
        offset: TapeOffset,
        event: EventId,
        site: DrawSite,
        len: u32,
        value: u64,
    ) {
        self.made += 1;
        if !self.keeps(offset.get(), len) {
            return;
        }
        self.records.push(DrawRecord {
            offset: offset.get(),
            event: event.get(),
            value,
            site: site as u16,
            pad: 0,
            len,
        });
    }

    /// Records an odds draw: the bytes it read, as any draw, and whether it fired.
    ///
    /// The record holds the bytes; the outcome is kept beside it for the reducer.
    pub fn push_odds(
        &mut self,
        offset: TapeOffset,
        event: EventId,
        site: DrawSite,
        drawn: u64,
        fired: bool,
    ) {
        if fired && self.keeps(offset.get(), 2) {
            self.fired.push(self.records.len());
        }
        self.push(offset, event, site, 2, drawn);
    }

    /// Whether the record at `index` is an odds draw that fired, where this run recorded it.
    ///
    /// Always false for a journal read back from bytes, which do not say.
    #[must_use]
    pub fn fired(&self, index: usize) -> bool {
        self.fired.binary_search(&index).is_ok()
    }

    /// The records kept, in the order they were drawn: every draw, unless the journal was made
    /// [`keeping`](Self::keeping) fewer.
    #[inline]
    #[must_use]
    pub fn records(&self) -> &[DrawRecord] {
        &self.records
    }

    /// How many draws were made, kept or not.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.made
    }

    /// Whether nothing was drawn.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.made == 0
    }

    /// The whole journal as the bytes of the `DRAW_JOURNAL` bundle section.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.records.len() * DRAW_RECORD_BYTES);
        for r in &self.records {
            out.extend_from_slice(&r.to_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_at_a_fixed_width() {
        let r = DrawRecord {
            offset: 0x0102_0304_0506_0708,
            event: 42,
            value: 7,
            site: DrawSite::SchedPick as u16,
            pad: 0,
            len: 4096,
        };
        let bytes = r.to_bytes();
        assert_eq!(bytes.len(), DRAW_RECORD_BYTES);
        assert_eq!(DrawRecord::from_bytes(&bytes).expect("a record we wrote"), r);
    }

    /// A non-zero reserved field is refused.
    #[test]
    fn a_reserved_field_that_is_not_zero_is_named_rather_than_carried() {
        let mut bytes = DrawRecord {
            offset: 0,
            event: 0,
            value: 0,
            site: DrawSite::SchedPick as u16,
            pad: 0,
            len: 1,
        }
        .to_bytes();
        bytes[26] = 1;
        assert_eq!(DrawRecord::from_bytes(&bytes), Err(TapeError::ReservedNotZero));
    }

    /// What fired is kept beside the records, so a read-back journal equals the written one.
    #[test]
    fn an_odds_draw_keeps_its_bytes_and_says_whether_it_fired() {
        let mut j = Journal::new();
        j.push_odds(TapeOffset(0), EventId(1), DrawSite::NodeFault, 0x1234, false);
        j.push_odds(TapeOffset(2), EventId(2), DrawSite::NodeFault, 7, true);
        assert_eq!(j.records()[1].value, 7);
        assert!(!j.fired(0));
        assert!(j.fired(1));
        let back = Journal::from_records(j.records().to_vec());
        assert_eq!(back, j);
        assert!(!back.fired(1), "a stored journal cannot say what fired");
    }

    #[test]
    fn journal_serialises_as_a_flat_array() {
        let mut j = Journal::new();
        j.push(TapeOffset(0), EventId(1), DrawSite::SchedPick, 1, 3);
        j.push(TapeOffset(1), EventId(1), DrawSite::GuestRandom, 8, 0);
        assert_eq!(j.len(), 2);
        assert_eq!(j.to_bytes().len(), 2 * DRAW_RECORD_BYTES);
    }

    /// Every site code round-trips.
    #[test]
    fn a_stored_code_reads_back_as_the_site_that_wrote_it() {
        let sites = [
            DrawSite::SchedPick,
            DrawSite::SchedPriority,
            DrawSite::SchedSwitchPoint,
            DrawSite::SyncWake,
            DrawSite::NetDeliver,
            DrawSite::NetDelay,
            DrawSite::NetReorder,
            DrawSite::NetPartition,
            DrawSite::NetSocket,
            DrawSite::NetCorrupt,
            DrawSite::DiskLatency,
            DrawSite::DiskTear,
            DrawSite::DiskReorder,
            DrawSite::DiskError,
            DrawSite::NodeFault,
            DrawSite::ClockSkew,
            DrawSite::MemoryExhausted,
            DrawSite::GuestRandom,
            DrawSite::SdkRandom,
            DrawSite::SdkFaultPoint,
        ];
        for site in sites {
            assert_eq!(DrawSite::from_code(site as u16), Some(site), "{}", site.name());
            assert!(!site.name().is_empty());
        }
        assert_eq!(DrawSite::from_code(999), None, "an unknown code is not guessed at");
    }

    #[test]
    fn site_values_are_the_frozen_ones() {
        // Stored journals carry these values.
        assert_eq!(DrawSite::SchedPick as u16, 1);
        assert_eq!(DrawSite::SyncWake as u16, 4);
        assert_eq!(DrawSite::NetDeliver as u16, 10);
        assert_eq!(DrawSite::DiskTear as u16, 21);
        assert_eq!(DrawSite::GuestRandom as u16, 40);
        assert_eq!(DrawSite::SdkFaultPoint as u16, 42);
    }
}
