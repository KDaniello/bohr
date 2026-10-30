//! The event log, the trace digest and the `.bohr` scenario bundle.
//!
//! The digest is the operational definition of "the same run": a byte-identical projection of the
//! event stream, with host-derived values excluded so that one seed produces one digest on any
//! machine. The bundle is what that run is stored and exchanged as — self-contained, and refusing
//! to be replayed under components that did not produce it.
//!
//! Formats are normative in `docs/API.md` §4 and frozen by [`BUNDLE_FORMAT`].
//!
//! ## Layout
//!
//! - [`Event`] — one recorded fact, and its canonical projection
//! - [`TraceWriter`] — the framed log, and the running digest
//! - [`Bundle`] — the container, its sections and its manifest

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bundle;
#[cfg(feature = "chaos-wall-clock-digest")]
mod chaos;
mod event;
mod log;
mod redact;
mod syscalls;

pub use bundle::{
    derive_run_id, frame_streams, group_digest, manifest_for, now_utc, read_streams, BinaryRecord,
    Bundle, BundleWriter, Counters, DeterminismReport, ExecutionRecord, Group, HostProfile,
    Manifest, ManifestInputs, Outcome, PolicyRecord, ProjectionRecord, Provenance, SectionId,
    SingleExecution, BUNDLE_HEADER_BYTES, BUNDLE_MAGIC, SECTION_ENTRY_BYTES,
};
pub use event::{
    short_digest, AssertionKind, DiskOpKind, Endpoint, Event, EventKind, ExitStatus, FaultKind,
    FaultTarget, NodeState, PacketFate, SwitchReason, DURABILITY_CODE, LIVENESS_CODE,
};
pub use log::{digest_of, read_events, read_events_lossy, TraceWriter};
pub use redact::redact;
pub use syscalls::{named_syscalls, syscall_args, syscall_name, Arg};

/// Version of the bundle layout.
pub const BUNDLE_FORMAT: u16 = 2;

/// Version of the SDK record schema recorded in a manifest.
pub const SDK_SCHEMA: u16 = 1;

/// Failures that originate in the trace layer.
#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    /// `BOHR-4001` — the container is not a bundle.
    #[error("BOHR-4001: not a bohr bundle")]
    NotABundle,

    /// `BOHR-4001` — written by a newer tool than this one.
    #[error("BOHR-4001: bundle format {found} is newer than this tool supports ({supported})")]
    UnsupportedBundleFormat {
        /// Format recorded in the header.
        found: u16,
        /// Format this build implements.
        supported: u16,
    },

    /// `BOHR-4001` — one section number appears twice in the table.
    #[error("BOHR-4001: section {id} appears more than once")]
    DuplicateSection {
        /// The repeated section number.
        id: u32,
    },

    /// `BOHR-4002` — a section does not hash to what the table claims.
    #[error("BOHR-4002: section {id} hash mismatch (table {expected}, computed {computed})")]
    SectionHashMismatch {
        /// Which section.
        id: u32,
        /// Digest recorded in the table.
        expected: bohr_types::Digest,
        /// Digest of the bytes actually present.
        computed: bohr_types::Digest,
    },

    /// `BOHR-4002` — the container ends before its own table says it should.
    #[error("BOHR-4002: bundle truncated at offset {offset}")]
    BundleTruncated {
        /// Where the container stopped being readable.
        offset: u64,
    },

    /// `BOHR-4002` — a required section is absent.
    #[error("BOHR-4002: bundle is missing required section {id}")]
    MissingSection {
        /// Which section.
        id: u32,
    },

    /// `BOHR-4002` — a section is present but cannot be read back.
    ///
    /// Damage to a packed payload stops it decoding before its hash can be checked, so the
    /// promise that a damaged bundle names its bad part is kept here rather than there.
    #[error("BOHR-4002: section {id} cannot be read: {reason}")]
    SectionUnreadable {
        /// Which section.
        id: u32,
        /// What went wrong.
        reason: String,
    },

    /// `BOHR-4001` — a section is encoded in a way this build cannot read.
    #[error("BOHR-4001: section {id} carries unknown flags {flags:#x}")]
    UnknownSectionFlags {
        /// Which section.
        id: u32,
        /// The flag word found.
        flags: u32,
    },

    /// `BOHR-4005` — the tape ABI is not the one this build interprets.
    ///
    /// Refused rather than replayed: a run derived from differently interpreted bytes is a
    /// different run, and one that would look self-consistent.
    #[error("BOHR-4005: bundle tape ABI {found} is not this build's ({supported})")]
    TapeAbiMismatch {
        /// ABI recorded in the manifest.
        found: u16,
        /// ABI this build implements.
        supported: u16,
    },

    /// `BOHR-4006` — produced by a different world model or oracle.
    #[error(
        "BOHR-4006: bundle was produced by {recorded}, this tool is configured with {configured}"
    )]
    ForeignProvenance {
        /// Components recorded in the manifest.
        recorded: String,
        /// Components this tool is running.
        configured: String,
    },

    /// `BOHR-4007` — holds several executions where one was expected.
    #[error("BOHR-4007: bundle holds {count} executions; this command handles one")]
    MultipleExecutions {
        /// How many the bundle carries.
        count: u32,
    },

    /// `BOHR-4002` — the event log stops mid-record.
    #[error("BOHR-4002: event log truncated at offset {offset}")]
    TruncatedAt {
        /// Where the log stopped being readable.
        offset: u64,
    },

    /// `BOHR-4002` — a record's leading and trailing lengths disagree.
    #[error("BOHR-4002: event frame at offset {offset} has mismatched length markers")]
    FrameMismatch {
        /// Where the damaged frame begins.
        offset: u64,
    },

    /// `BOHR-4002` — a length field exceeds what any record may be.
    #[error("BOHR-4002: event record length exceeds the permitted maximum")]
    RecordTooLarge,

    /// `BOHR-5001` — an event could not be encoded.
    #[error("BOHR-5001: cannot encode event: {0}")]
    Encode(String),

    /// `BOHR-4002` — the stream table disagrees with the manifest about how many runs are here.
    ///
    /// Distinct from [`Self::MultipleExecutions`], which is a bundle that genuinely holds several
    /// and a reader that handles one. This is damage: the two halves of one file do not agree.
    #[error("BOHR-4002: bundle declares {declared} executions and carries {found} streams")]
    FrameCountMismatch {
        /// How many the manifest says there are.
        declared: u32,
        /// How many streams the table actually holds.
        found: u32,
    },

    /// `BOHR-4008` — a redaction could not prove it removed what it was asked to.
    ///
    /// Refused rather than written. A redacted bundle is handed to somebody outside the room, and
    /// one that passed through what it did not understand would be redacting whatever it happened
    /// to recognise — which is the same as not redacting, except that it says otherwise.
    #[error("BOHR-4008: redaction cannot be proven complete: {reason}")]
    RedactionIncomplete {
        /// What could not be removed, or could not be understood well enough to remove.
        reason: String,
    },

    /// `BOHR-4002` — stored bytes could not be decoded.
    #[error("BOHR-4002: cannot decode: {0}")]
    Decode(String),

    /// Underlying I/O failure.
    #[error("BOHR-4002: trace I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
