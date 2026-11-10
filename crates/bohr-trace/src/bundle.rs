//! The `.bohr` scenario bundle: a section table followed by section payloads.
//!
//! Sections are hashed independently, so a damaged bundle names the bad
//! part. Layout is normative in `docs/API.md` §4.1.

use std::{borrow::Cow, collections::BTreeMap};

use bohr_types::{Digest, OracleId, Seed, WorldModelId};
use serde::{Deserialize, Serialize};

use crate::{BUNDLE_FORMAT, SDK_SCHEMA, TraceError};

/// First eight bytes of a bundle.
pub const BUNDLE_MAGIC: [u8; 8] = *b"BOHRBNDL";

/// Size of the bundle header, before the section table.
pub const BUNDLE_HEADER_BYTES: usize = 32;

/// Size of one section table entry.
pub const SECTION_ENTRY_BYTES: usize = 64;

/// Section flag: the payload is a zstd frame (`docs/API.md` §4.1, bit 0).
const SECTION_FLAG_ZSTD: u32 = 1;

/// Below this a frame costs more than it saves, so a small section is stored as it is.
const COMPRESS_THRESHOLD: usize = 512;

/// Compression level, fixed so every build writes the same bytes.
const COMPRESS_LEVEL: i32 = 3;

/// The largest a section may unpack to.
///
/// Sections are hashed in memory. Far above any real section; it bounds hostile files.
const MAX_SECTION_BYTES: u64 = 1 << 30;

/// Domain separator for the run identifier.
const RUN_ID_CONTEXT: &[u8] = b"bohr/bundle/run-id/v1";

/// Sections a bundle can carry. Numbering is part of the format.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u32)]
pub enum SectionId {
    /// Run metadata, as JSON.
    Manifest = 1,
    /// A complete `.tape` container.
    Tape = 2,
    /// The framed event log.
    Events = 3,
    /// SDK records, as JSON Lines.
    SdkRecords = 4,
    /// The draw journal.
    DrawJournal = 5,
    /// Edge coverage bitmap.
    Coverage = 6,
    /// Fingerprints of every mapped executable object, as JSON.
    Binaries = 7,
    /// The profile presented to the guest, as JSON.
    HostProfile = 8,
    /// Effective merged configuration.
    Config = 9,
    /// Free-form notes.
    Notes = 10,
    /// What the prepared filesystem a run began from held: paths, lengths and digests.
    ///
    /// The list only, so the bundle stays attachable; a replay against another state is refused.
    StateManifest = 11,
    /// What each execution of a differential run was, and what it produced.
    ///
    /// Present only where one tape drove more than one implementation. See `docs/API.md` §4.2.
    Executions = 12,
}

/// One implementation's run, within a bundle that holds several.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ExecutionRecord {
    /// Position in the group, and the index this execution's streams are tagged with.
    pub index: u32,
    /// What the configuration called this implementation.
    pub name: String,
    /// Digest of this execution's own event stream.
    pub trace_digest: String,
    /// What this execution amounted to on its own.
    pub outcome: Outcome,
    /// The values it was compared by.
    pub projections: Vec<ProjectionRecord>,
}

/// One value a run is compared by.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ProjectionRecord {
    /// What the configuration called it.
    pub label: String,
    /// Digest of the bytes, which is what is compared.
    pub digest: String,
    /// How many bytes there were.
    pub len: u64,
    /// Whether the value ran past what may be held.
    ///
    /// A truncated value is reported and never compared.
    pub truncated: bool,
}

/// Frames N per-execution streams into one section payload.
///
/// `u32` index, `u64` length, bytes: ascending, one frame per execution, empty frames included
/// (`docs/API.md` §4.2).
#[must_use]
pub fn frame_streams(streams: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, bytes) in streams.iter().enumerate() {
        out.extend_from_slice(&(index as u32).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

/// Reads back what [`frame_streams`] wrote.
///
/// # Errors
///
/// Refuses truncation, indices not ascending from zero, and a frame count other than `expected`.
pub fn read_streams(payload: &[u8], expected: u32) -> Result<Vec<Vec<u8>>, TraceError> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < payload.len() {
        if payload.len() - at < 12 {
            return Err(TraceError::BundleTruncated { offset: at as u64 });
        }
        // Both widths were checked above.
        let index = u32::from_le_bytes(payload[at..at + 4].try_into().unwrap_or([0; 4]));
        let len = u64::from_le_bytes(payload[at + 4..at + 12].try_into().unwrap_or([0; 8]));
        at += 12;

        let len =
            usize::try_from(len).map_err(|_| TraceError::BundleTruncated { offset: at as u64 })?;
        if index as usize != out.len() || payload.len() - at < len {
            return Err(TraceError::BundleTruncated { offset: at as u64 });
        }
        out.push(payload[at..at + len].to_vec());
        at += len;
    }
    // A frame table disagreeing with the manifest.
    if out.len() != expected as usize {
        return Err(TraceError::FrameCountMismatch { declared: expected, found: out.len() as u32 });
    }
    Ok(out)
}

/// What a differential run's digest is: the ordered per-execution digests and what each produced.
///
/// The projections are part of the digest, since they are what a differential run compares.
#[must_use]
pub fn group_digest(executions: &[ExecutionRecord]) -> Digest {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"bohr/bundle/group/v1");
    canonical.extend_from_slice(&(executions.len() as u32).to_le_bytes());
    for execution in executions {
        canonical.extend_from_slice(execution.trace_digest.as_bytes());
        canonical.extend_from_slice(&(execution.projections.len() as u32).to_le_bytes());
        // Sorted by label (unstable, so equal labels never keep file order) and length-prefixed,
        // since TOML strings can hold a zero byte.
        let mut sorted = execution.projections.clone();
        sorted.sort_unstable_by(|a, b| a.label.cmp(&b.label).then(a.digest.cmp(&b.digest)));
        for projection in sorted {
            canonical.extend_from_slice(&(projection.label.len() as u64).to_le_bytes());
            canonical.extend_from_slice(projection.label.as_bytes());
            canonical.extend_from_slice(projection.digest.as_bytes());
        }
    }
    Digest::of(&canonical)
}

/// Why a run ended.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Outcome {
    /// Nothing was found.
    Clean,
    /// An asserted property did not hold.
    PropertyViolated(String),
    /// The guest died.
    GuestCrashed(String),
    /// The guest stopped making progress.
    GuestHung(String),
    /// A cluster that could talk to itself again went on committing nothing.
    ///
    /// Separate from `PropertyViolated`, whose empty assertion set would match any violation.
    LivenessViolated(String),
    /// A node answered while what it had written was not durable, and came back without it.
    ///
    /// Separate for the same reason, and a different defect from liveness.
    DurabilityViolated(String),
    /// A probe asked a node's own API, and the answers broke the property it watched.
    ///
    /// Separate for the same reason: the guest asserted nothing.
    ProbeViolated(String),
    /// A syscall outside the support table was reached.
    UnsupportedSyscall(String),
    /// Independent implementations produced different results.
    ImplementationsDisagreed(String),
    /// A defect in the tool.
    InternalError(String),
    /// The run was stopped at an event somebody named, and handed over.
    ///
    /// Only from `bohr replay --break-at`, which writes no bundle, so the format is unchanged.
    StoppedOnRequest(String),
}

/// Which components produced a run.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Provenance {
    /// World model that answered the guest.
    pub world_model: WorldModelId,
    /// Oracle that judged the run.
    pub oracle: OracleId,
    /// Backend that executed the guest.
    pub backend: String,
}

/// How far determinism was actually guaranteed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct DeterminismReport {
    /// `full` or `degraded`.
    pub level: String,
    /// Named causes, present only when degraded.
    pub reasons: Vec<String>,
}

/// What a run amounted to.
///
/// Lets a gate tell a richer run from a shorter one with a matching digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Counters {
    /// Events recorded.
    pub events: u64,
    /// Syscalls the world answered.
    pub syscalls: u64,
    /// Draws taken from the tape.
    pub draws: u64,
    /// Virtual time reached.
    pub ticks: u64,
    /// Tasks that existed.
    pub tasks: u64,
    /// Nodes the run was configured with.
    pub nodes: u64,
}

/// How execution was interleaved, as the bundle records it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PolicyRecord {
    /// Name of the scheduling policy.
    pub scheduler: String,
    /// Depth, where the policy has one.
    pub pct_depth: Option<u32>,
    /// Which stops the scheduler was offered a choice at.
    pub switch_points: String,
}

/// Run metadata, stored as the `Manifest` section.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// Bundle layout version.
    pub bundle_format: u16,
    /// Draw ABI the tape was interpreted under.
    pub tape_abi: u16,
    /// SDK record schema version.
    pub sdk_schema: u16,
    /// Version of the tool that wrote this.
    pub tool_version: String,
    /// Identifier of this bundle.
    pub run_id: String,
    /// When the bundle was written, RFC 3339 in UTC.
    ///
    /// Outside the run identifier and the digest, so a reproducible build can pin it.
    pub created_utc: String,
    /// Seed the run was derived from.
    pub seed: u64,
    /// Why the run ended.
    pub outcome: Outcome,
    /// The event a finding points at, where it points at one.
    pub outcome_event: Option<u64>,
    /// What the run amounted to, in numbers a reader compares bundles by.
    pub counters: Counters,
    /// How execution was interleaved.
    pub policy: PolicyRecord,
    /// Whether determinism held, and why not if it did not.
    pub determinism: DeterminismReport,
    /// Digest of the canonical event stream.
    pub trace_digest: String,
    /// Which components produced the run.
    pub provenance: Provenance,
    /// How many executions one tape drove.
    pub executions: u32,
    /// Identifier of the bundle this was reduced from.
    pub reduced_from: Option<String>,
    /// The prepared filesystem this run began from, where it began from one.
    ///
    /// Checked before a replay, like the binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_digest: Option<String>,
}

impl Manifest {
    /// Refuses a bundle produced by different components.
    ///
    /// World models answer syscalls differently, so they are never mixed.
    pub fn check_provenance(
        &self,
        world: &WorldModelId,
        oracle: &OracleId,
    ) -> Result<(), TraceError> {
        if &self.provenance.world_model != world || &self.provenance.oracle != oracle {
            return Err(TraceError::ForeignProvenance {
                recorded: format!("{}/{}", self.provenance.world_model, self.provenance.oracle),
                configured: format!("{world}/{oracle}"),
            });
        }
        Ok(())
    }

    /// Refuses a multi-execution bundle where only one execution can be handled.
    ///
    /// Reading only the first stream would discard the disagreement.
    pub fn require_single_execution(&self) -> Result<(), TraceError> {
        if self.executions != 1 {
            return Err(TraceError::MultipleExecutions { count: self.executions });
        }
        Ok(())
    }

    /// Refuses a bundle whose tape this build would interpret differently.
    ///
    /// Another draw ABI reads the same bytes as another run. See `docs/API.md` §9.
    pub fn check_tape_abi(&self, supported: u16) -> Result<(), TraceError> {
        if self.tape_abi != supported {
            return Err(TraceError::TapeAbiMismatch { found: self.tape_abi, supported });
        }
        Ok(())
    }
}

/// What the guest was shown of the machine, as recorded in `HostProfile` (`docs/API.md` §4.5).
///
/// Every field is what the guest was shown, never the real host.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct HostProfile {
    /// Backend that executed the guest.
    pub backend: String,
    /// What progress was counted in.
    pub progress_unit: String,
    /// Page size the guest was shown.
    pub page_size: u32,
    /// Processors the guest was told it had.
    pub cpu_count_presented: u32,
    /// Kernel release the guest was told it ran on.
    pub kernel_release_presented: String,
    /// Whether address randomisation was disabled.
    pub aslr_disabled: bool,
    /// Whether the auxiliary vector's randomness was zeroed.
    pub at_random_zeroed: bool,
    /// Whether the timestamp counter was trapped.
    pub traps_rdtsc: bool,
    /// Whether processor identification was trapped.
    pub traps_cpuid: bool,
    /// Whether hardware random instructions were trapped.
    pub traps_rdrand: bool,
}

/// One object the guest ran, and the fingerprint a replay checks it against (`docs/API.md` §4.5).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BinaryRecord {
    /// Where the object was found.
    pub path: String,
    /// `blake3` of its bytes.
    pub blake3: String,
    /// What it was to the run: the program, its loader, or a library it mapped.
    pub role: String,
    /// The arguments after the program's own name, where it was given any.
    ///
    /// Arguments sit on the initial stack, so a replay passes the same ones. Absent means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// Packs a section, or declines where packing would not pay.
fn compress(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < COMPRESS_THRESHOLD {
        return None;
    }
    let packed = zstd::bulk::compress(bytes, COMPRESS_LEVEL).ok()?;
    (packed.len() < bytes.len()).then_some(packed)
}

/// Unpacks a section, refusing anything that does not unpack to the length recorded for it.
///
/// The recorded length is never trusted with an allocation: unpacking is incremental and stops
/// one byte past it.
fn decompress(payload: &[u8], original: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    if original > MAX_SECTION_BYTES {
        return Err(format!(
            "claims {original} bytes, past the {MAX_SECTION_BYTES} a reader holds"
        ));
    }
    let mut out = Vec::new();
    let decoder = zstd::stream::read::Decoder::new(payload).map_err(|e| e.to_string())?;
    decoder.take(original.saturating_add(1)).read_to_end(&mut out).map_err(|e| e.to_string())?;
    if out.len() as u64 != original {
        return Err(format!("unpacked to {} bytes, {original} recorded", out.len()));
    }
    Ok(out)
}

/// Derives the identifier of a run.
///
/// Derived, since the supervisor uses no randomness. See `docs/API.md` §4.1.
#[must_use]
pub fn derive_run_id(seed: Seed, trace_digest: Digest) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(RUN_ID_CONTEXT);
    hasher.update(&seed.get().to_le_bytes());
    hasher.update(trace_digest.as_bytes());
    let full = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&full.as_bytes()[..16]);
    out
}

/// Assembles a bundle.
#[derive(Debug, Default)]
pub struct BundleWriter {
    sections: BTreeMap<u32, Vec<u8>>,
}

impl BundleWriter {
    /// An empty bundle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a section.
    pub fn section(&mut self, id: SectionId, bytes: Vec<u8>) -> &mut Self {
        self.sections.insert(id as u32, bytes);
        self
    }

    /// Adds or replaces a section by raw number, preserving sections this build does not know.
    pub fn raw_section(&mut self, id: u32, bytes: Vec<u8>) -> &mut Self {
        self.sections.insert(id, bytes);
        self
    }

    /// Serialises the container.
    ///
    /// # Panics
    ///
    /// Panics on more sections than the header's count can hold.
    #[must_use]
    pub fn finish(&self, run_id: [u8; 16]) -> Vec<u8> {
        let count = self.sections.len();
        assert!(
            u16::try_from(count).is_ok(),
            "a bundle cannot hold more sections than its header can count"
        );
        let table_bytes = count * SECTION_ENTRY_BYTES;
        let mut out = Vec::with_capacity(BUNDLE_HEADER_BYTES + table_bytes);

        out.extend_from_slice(&BUNDLE_MAGIC);
        out.extend_from_slice(&BUNDLE_FORMAT.to_le_bytes());
        out.extend_from_slice(&(count as u16).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&run_id);
        debug_assert_eq!(out.len(), BUNDLE_HEADER_BYTES);

        let mut payload_offset = (BUNDLE_HEADER_BYTES + table_bytes) as u64;
        // Borrowed when stored as is, so a large trace is not copied.
        let mut payloads: Vec<Cow<'_, [u8]>> = Vec::with_capacity(count);
        for (id, bytes) in &self.sections {
            let original = bytes.len() as u64;
            let hash = Digest::of(bytes);
            // Compressed only above the threshold and only if it shrinks.
            let (flags, payload) = match compress(bytes) {
                Some(packed) => (SECTION_FLAG_ZSTD, Cow::Owned(packed)),
                None => (0, Cow::Borrowed(bytes.as_slice())),
            };
            let stored = payload.len() as u64;
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&payload_offset.to_le_bytes());
            out.extend_from_slice(&stored.to_le_bytes());
            out.extend_from_slice(&original.to_le_bytes());
            // Hashed over the original bytes.
            out.extend_from_slice(hash.as_bytes());
            payload_offset += stored;
            payloads.push(payload);
        }
        // Reserve the known total to avoid regrowth.
        out.reserve(payload_offset as usize - out.len());
        for bytes in payloads {
            out.extend_from_slice(&bytes);
        }
        out
    }
}

/// A parsed bundle.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Bundle {
    /// Identifier recorded in the header.
    pub run_id: [u8; 16],
    /// Layout version recorded in the header.
    pub format: u16,
    sections: BTreeMap<u32, Vec<u8>>,
}

impl Bundle {
    /// Parses a container, verifying every section against its recorded hash.
    pub fn parse(bytes: &[u8]) -> Result<Self, TraceError> {
        if bytes.len() < BUNDLE_HEADER_BYTES {
            return Err(TraceError::BundleTruncated { offset: 0 });
        }
        if bytes[0..8] != BUNDLE_MAGIC {
            return Err(TraceError::NotABundle);
        }
        let format = u16::from_le_bytes([bytes[8], bytes[9]]);
        if format > BUNDLE_FORMAT {
            return Err(TraceError::UnsupportedBundleFormat {
                found: format,
                supported: BUNDLE_FORMAT,
            });
        }
        // A non-zero reserved field is refused, as in the tape container.
        if bytes[12..16].iter().any(|b| *b != 0) {
            return Err(TraceError::BundleTruncated { offset: 12 });
        }
        let count = u16::from_le_bytes([bytes[10], bytes[11]]) as usize;
        let mut run_id = [0u8; 16];
        run_id.copy_from_slice(&bytes[16..32]);

        let table_end = BUNDLE_HEADER_BYTES + count * SECTION_ENTRY_BYTES;
        if bytes.len() < table_end {
            return Err(TraceError::BundleTruncated { offset: bytes.len() as u64 });
        }

        let mut sections = BTreeMap::new();
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(count);
        for i in 0..count {
            let e = &bytes[BUNDLE_HEADER_BYTES + i * SECTION_ENTRY_BYTES..][..SECTION_ENTRY_BYTES];
            let id = u32::from_le_bytes(e[0..4].try_into().unwrap_or_default());
            let flags = u32::from_le_bytes(e[4..8].try_into().unwrap_or_default());
            let offset = u64::from_le_bytes(e[8..16].try_into().unwrap_or_default());
            let stored = u64::from_le_bytes(e[16..24].try_into().unwrap_or_default());
            let original = u64::from_le_bytes(e[24..32].try_into().unwrap_or_default());
            let expected = Digest(e[32..64].try_into().unwrap_or_default());
            // An unknown flag is an unreadable encoding.
            if flags & !SECTION_FLAG_ZSTD != 0 {
                return Err(TraceError::UnknownSectionFlags { id, flags });
            }

            let start =
                usize::try_from(offset).map_err(|_| TraceError::BundleTruncated { offset })?;
            let len =
                usize::try_from(stored).map_err(|_| TraceError::BundleTruncated { offset })?;
            let end = start
                .checked_add(len)
                .filter(|e| *e <= bytes.len())
                .ok_or(TraceError::BundleTruncated { offset })?;

            let raw = &bytes[start..end];
            let payload = if flags & SECTION_FLAG_ZSTD != 0 {
                decompress(raw, original)
                    .map_err(|reason| TraceError::SectionUnreadable { id, reason })?
            } else {
                if stored != original {
                    return Err(TraceError::BundleTruncated { offset });
                }
                raw.to_vec()
            };
            let computed = Digest::of(&payload);
            if computed != expected {
                return Err(TraceError::SectionHashMismatch { id, expected, computed });
            }
            // Duplicate section numbers are refused.
            if sections.insert(id, payload).is_some() {
                return Err(TraceError::DuplicateSection { id });
            }
            spans.push((start, end));
        }

        // Every byte after the table belongs to exactly one section: no gaps, no overlaps.
        spans.sort_unstable();
        let mut reach = table_end;
        for (start, end) in spans {
            if start != reach {
                return Err(TraceError::BundleTruncated { offset: start as u64 });
            }
            reach = end;
        }
        if reach != bytes.len() {
            return Err(TraceError::BundleTruncated { offset: reach as u64 });
        }

        Ok(Self { run_id, format, sections })
    }

    /// The one execution this bundle holds, refusing a bundle that holds several.
    ///
    /// Enforces `docs/API.md` §4.2: a single-execution reader refuses `executions > 1`.
    ///
    /// # Errors
    ///
    /// Refuses a bundle with no manifest, and one holding more than one execution.
    pub fn single(&self) -> Result<SingleExecution<'_>, TraceError> {
        let manifest = self.manifest()?;
        manifest.require_single_execution()?;
        Ok(SingleExecution { bundle: self })
    }

    /// Every execution this bundle holds, however many that is.
    ///
    /// # Errors
    ///
    /// Refuses a bundle with no manifest, and one whose framed streams disagree with the number of
    /// executions its manifest declares.
    pub fn group(&self) -> Result<Group<'_>, TraceError> {
        let manifest = self.manifest()?;
        Ok(Group { bundle: self, executions: manifest.executions })
    }

    /// A section, if present.
    #[must_use]
    pub fn section(&self, id: SectionId) -> Option<&[u8]> {
        self.raw_section(id as u32)
    }

    /// A section by raw number, including one this build does not understand.
    #[must_use]
    pub fn raw_section(&self, id: u32) -> Option<&[u8]> {
        self.sections.get(&id).map(Vec::as_slice)
    }

    /// Every section number present, ascending.
    pub fn section_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.sections.keys().copied()
    }

    /// Parses the manifest.
    pub fn manifest(&self) -> Result<Manifest, TraceError> {
        let raw = self
            .section(SectionId::Manifest)
            .ok_or(TraceError::MissingSection { id: SectionId::Manifest as u32 })?;
        serde_json::from_slice(raw).map_err(|e| TraceError::Decode(e.to_string()))
    }

    /// Rebuilds a writer holding every section, including unrecognised ones.
    ///
    /// Unknown sections are kept, so an older tool never loses data.
    #[must_use]
    pub fn to_writer(&self) -> BundleWriter {
        let mut w = BundleWriter::new();
        for (id, bytes) in &self.sections {
            w.raw_section(*id, bytes.clone());
        }
        w
    }
}

/// A bundle that holds exactly one execution, as the one thing it holds.
///
/// Obtained only from [`Bundle::single`], which refuses several executions.
#[derive(Clone, Copy, Debug)]
pub struct SingleExecution<'a> {
    bundle: &'a Bundle,
}

impl SingleExecution<'_> {
    /// The framed event log.
    #[must_use]
    pub fn events(&self) -> Option<&[u8]> {
        self.bundle.section(SectionId::Events)
    }

    /// What the guest wrote about itself.
    #[must_use]
    pub fn sdk_records(&self) -> Option<&[u8]> {
        self.bundle.section(SectionId::SdkRecords)
    }

    /// The edge coverage map.
    #[must_use]
    pub fn coverage(&self) -> Option<&[u8]> {
        self.bundle.section(SectionId::Coverage)
    }
}

/// A bundle read as every execution it holds.
#[derive(Clone, Copy, Debug)]
pub struct Group<'a> {
    bundle: &'a Bundle,
    executions: u32,
}

impl Group<'_> {
    /// How many executions one tape drove.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.executions
    }

    /// Whether this bundle holds no executions at all, which no written bundle does.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.executions == 0
    }

    /// What each execution was, and what it produced.
    ///
    /// # Errors
    ///
    /// Refuses a section that does not parse.
    pub fn records(&self) -> Result<Vec<ExecutionRecord>, TraceError> {
        let Some(raw) = self.bundle.section(SectionId::Executions) else {
            return Ok(Vec::new());
        };
        serde_json::from_slice(raw).map_err(|e| TraceError::Decode(e.to_string()))
    }

    /// One event log per execution, in index order.
    ///
    /// # Errors
    ///
    /// Refuses framing that disagrees with the number of executions the manifest declares.
    pub fn events(&self) -> Result<Vec<Vec<u8>>, TraceError> {
        self.streams(SectionId::Events)
    }

    /// One record stream per execution, in index order.
    ///
    /// # Errors
    ///
    /// Refuses framing that disagrees with the number of executions the manifest declares.
    pub fn sdk_records(&self) -> Result<Vec<Vec<u8>>, TraceError> {
        self.streams(SectionId::SdkRecords)
    }

    fn streams(&self, id: SectionId) -> Result<Vec<Vec<u8>>, TraceError> {
        let Some(raw) = self.bundle.section(id) else {
            return Ok(Vec::new());
        };
        // A single execution is unframed.
        if self.executions <= 1 {
            return Ok(vec![raw.to_vec()]);
        }
        read_streams(raw, self.executions)
    }
}

/// Everything a manifest needs that is not derived from the run itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ManifestInputs {
    /// The run this one was reduced from, where it was reduced from anything.
    ///
    /// Names the finding a reduced bundle came from.
    pub reduced_from: Option<String>,
    /// The prepared filesystem the run began from, where it began from one.
    pub state_digest: Option<String>,
    /// Seed the run was derived from.
    pub seed: Seed,
    /// Digest of the canonical event stream.
    pub trace_digest: Digest,
    /// Identifier of the bundle.
    pub run_id: [u8; 16],
    /// Which components produced the run.
    pub provenance: Provenance,
    /// Why the run ended.
    pub outcome: Outcome,
    /// The event a finding points at.
    pub outcome_event: Option<u64>,
    /// What the run amounted to.
    pub counters: Counters,
    /// How execution was interleaved.
    pub policy: PolicyRecord,
    /// Conditions under which determinism was not fully guaranteed.
    pub degraded_reasons: Vec<String>,
    /// When the bundle was written, RFC 3339 in UTC.
    ///
    /// Supplied by the caller (§8 rule 9), usually from [`now_utc`].
    pub created_utc: String,
}

/// A manifest for a single-execution run.
#[must_use]
pub fn manifest_for(inputs: ManifestInputs) -> Manifest {
    let level = if inputs.degraded_reasons.is_empty() { "full" } else { "degraded" };
    Manifest {
        bundle_format: BUNDLE_FORMAT,
        tape_abi: bohr_tape_abi(),
        sdk_schema: SDK_SCHEMA,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        run_id: hex16(&inputs.run_id),
        created_utc: inputs.created_utc,
        seed: inputs.seed.get(),
        outcome: inputs.outcome,
        outcome_event: inputs.outcome_event,
        counters: inputs.counters,
        policy: inputs.policy,
        determinism: DeterminismReport {
            level: level.to_owned(),
            reasons: inputs.degraded_reasons,
        },
        trace_digest: inputs.trace_digest.to_string(),
        provenance: inputs.provenance,
        executions: 1,
        reduced_from: inputs.reduced_from,
        state_digest: inputs.state_digest,
    }
}

/// The moment a bundle is being written, RFC 3339 in UTC.
///
/// `SOURCE_DATE_EPOCH` wins when set. Called by the command layer, which may read the clock, and
/// passed in [`ManifestInputs::created_utc`].
#[must_use]
#[allow(clippy::disallowed_methods)]
pub fn now_utc() -> String {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
        })
        .unwrap_or(0);
    format_rfc3339(epoch)
}

/// Formats seconds since the epoch as RFC 3339, by the civil-from-days algorithm.
///
/// Hand-rolled; this is the only date the tool formats.
fn format_rfc3339(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let time = seconds % 86_400;
    // Howard Hinnant's civil_from_days, with the era shifted so 0000-03-01 is day zero.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// The draw ABI this build records.
///
/// Duplicated from `bohr-tape`; `scripts/lint-rules.sh` checks they match.
const fn bohr_tape_abi() -> u16 {
    2
}

fn hex16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('?'));
        s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('?'));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance {
            world_model: WorldModelId::new("distributed", 1),
            oracle: OracleId::new("properties+crash", 1),
            backend: "null".to_owned(),
        }
    }

    fn sample_bundle() -> Vec<u8> {
        let digest = Digest::of(b"trace");
        let run_id = derive_run_id(Seed(7), digest);
        let manifest = manifest_for(ManifestInputs {
            reduced_from: None,
            state_digest: None,
            seed: Seed(7),
            trace_digest: digest,
            run_id,
            provenance: provenance(),
            outcome: Outcome::Clean,
            outcome_event: None,
            counters: Counters::default(),
            policy: PolicyRecord {
                scheduler: "random".to_owned(),
                pct_depth: None,
                switch_points: "syscall+futex".to_owned(),
            },
            degraded_reasons: Vec::new(),
            created_utc: now_utc(),
        });
        let mut w = BundleWriter::new();
        w.section(SectionId::Manifest, serde_json::to_vec(&manifest).expect("manifest"));
        w.section(SectionId::Tape, vec![1, 2, 3, 4]);
        w.section(SectionId::Events, vec![9; 100]);
        w.finish(run_id)
    }

    /// A recorded length of a terabyte is refused without allocating.
    #[test]
    fn a_section_claiming_an_impossible_length_is_refused() {
        // Large and repetitive, so it is compressed.
        let mut w = BundleWriter::new();
        w.section(SectionId::Manifest, vec![b'{'; 4096]);
        let mut bytes = w.finish([0; 16]);

        // The recorded original length sits at offset 24 of the first section table entry.
        let at = BUNDLE_HEADER_BYTES + 24;
        bytes[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());

        let err = Bundle::parse(&bytes).expect_err("an impossible length is not readable");
        assert!(
            matches!(err, TraceError::SectionUnreadable { .. }),
            "the refusal must name the section: {err:?}"
        );
    }

    /// A frame expanding past its recorded length is stopped during unpacking.
    #[test]
    fn a_section_that_expands_past_its_record_is_refused() {
        let packed = zstd::bulk::compress(&vec![0u8; 1 << 20], COMPRESS_LEVEL).expect("packs");
        let mut w = BundleWriter::new();
        w.section(SectionId::Manifest, vec![b'{'; 4096]);
        let mut bytes = w.finish([0; 16]);

        // Keep the recorded length modest and put a frame under it that unpacks to a megabyte.
        let entry = BUNDLE_HEADER_BYTES;
        let offset = u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().expect("offset"));
        let start = usize::try_from(offset).expect("an offset in range");
        bytes.truncate(start);
        bytes.extend_from_slice(&packed);
        bytes[entry + 16..entry + 24].copy_from_slice(&(packed.len() as u64).to_le_bytes());
        bytes[entry + 24..entry + 32].copy_from_slice(&4096u64.to_le_bytes());

        let err = Bundle::parse(&bytes).expect_err("a frame that overruns its record is refused");
        assert!(matches!(err, TraceError::SectionUnreadable { .. }), "{err:?}");
    }

    #[test]
    fn round_trips_with_every_section_intact() {
        let bytes = sample_bundle();
        let b = Bundle::parse(&bytes).expect("parse");
        assert_eq!(b.format, BUNDLE_FORMAT);
        assert_eq!(b.section(SectionId::Tape), Some(&[1u8, 2, 3, 4][..]));
        assert_eq!(b.section(SectionId::Events).map(<[u8]>::len), Some(100));
        let m = b.manifest().expect("manifest");
        assert_eq!(m.seed, 7);
        assert_eq!(m.executions, 1);
        assert_eq!(m.determinism.level, "full");
    }

    #[test]
    fn the_run_id_is_a_function_of_the_run() {
        let d = Digest::of(b"trace");
        assert_eq!(derive_run_id(Seed(7), d), derive_run_id(Seed(7), d));
        assert_ne!(derive_run_id(Seed(8), d), derive_run_id(Seed(7), d));
        assert_ne!(derive_run_id(Seed(7), Digest::of(b"other")), derive_run_id(Seed(7), d));
    }

    #[test]
    fn damage_to_any_section_is_named_not_ignored() {
        let bytes = sample_bundle();
        let table_end = BUNDLE_HEADER_BYTES + 3 * SECTION_ENTRY_BYTES;
        for i in table_end..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[i] ^= 0x01;
            // The hash or the encoding catches it; both name the section.
            match Bundle::parse(&damaged) {
                Err(
                    TraceError::SectionHashMismatch { .. } | TraceError::SectionUnreadable { .. },
                ) => {}
                other => panic!("damage at {i} produced {other:?}"),
            }
        }
    }

    #[test]
    fn truncation_at_every_offset_is_rejected() {
        let bytes = sample_bundle();
        for cut in 0..bytes.len() {
            assert!(Bundle::parse(&bytes[..cut]).is_err(), "cut at {cut} parsed as valid");
        }
    }

    /// Framing round-trips, empty streams included.
    #[test]
    fn framed_streams_come_back_as_they_went_in() {
        let streams = vec![b"first".to_vec(), Vec::new(), b"third".to_vec()];
        let framed = frame_streams(&streams);
        assert_eq!(read_streams(&framed, 3).expect("it reads"), streams);
    }

    /// A frame table that disagrees with the manifest is refused.
    #[test]
    fn framing_that_disagrees_with_the_manifest_is_refused() {
        let framed = frame_streams(&[b"first".to_vec(), b"second".to_vec()]);
        assert!(read_streams(&framed, 3).is_err(), "two frames are not three executions");
        assert!(
            read_streams(&framed[..framed.len() - 1], 2).is_err(),
            "a cut frame is not a frame"
        );
    }

    /// The group digest includes the projections but not their listed order.
    #[test]
    fn a_group_digest_covers_what_was_produced_and_not_how_it_was_listed() {
        let one = execution(&[("root", "aaa"), ("height", "bbb")]);
        let same = execution(&[("height", "bbb"), ("root", "aaa")]);
        let other = execution(&[("root", "ccc"), ("height", "bbb")]);

        assert_eq!(group_digest(std::slice::from_ref(&one)), group_digest(&[same]));
        assert_ne!(group_digest(std::slice::from_ref(&one)), group_digest(&[other]));
        assert_ne!(group_digest(std::slice::from_ref(&one)), group_digest(&[one.clone(), one]));
    }

    fn execution(projections: &[(&str, &str)]) -> ExecutionRecord {
        ExecutionRecord {
            index: 0,
            name: "impl".to_owned(),
            trace_digest: "blake3:1234".to_owned(),
            outcome: Outcome::Clean,
            projections: projections
                .iter()
                .map(|(label, digest)| ProjectionRecord {
                    label: (*label).to_owned(),
                    digest: (*digest).to_owned(),
                    len: 3,
                    truncated: false,
                })
                .collect(),
        }
    }

    #[test]
    fn unknown_sections_survive_a_rewrite() {
        let mut w = BundleWriter::new();
        w.section(SectionId::Tape, vec![1, 2, 3]);
        w.raw_section(9999, b"from a newer tool".to_vec());
        let bytes = w.finish([0u8; 16]);

        let parsed = Bundle::parse(&bytes).expect("parse");
        let rewritten = parsed.to_writer().finish(parsed.run_id);
        let again = Bundle::parse(&rewritten).expect("reparse");

        assert_eq!(again.raw_section(9999), Some(&b"from a newer tool"[..]));
        assert_eq!(again.section_ids().collect::<Vec<_>>(), vec![2, 9999]);
    }

    #[test]
    fn a_bundle_from_another_world_model_is_refused() {
        let bytes = sample_bundle();
        let m = Bundle::parse(&bytes).expect("parse").manifest().expect("manifest");

        assert!(
            m.check_provenance(
                &WorldModelId::new("distributed", 1),
                &OracleId::new("properties+crash", 1)
            )
            .is_ok()
        );

        let err = m
            .check_provenance(
                &WorldModelId::new("consensus", 1),
                &OracleId::new("properties+crash", 1),
            )
            .expect_err("a foreign model must be refused");
        assert!(matches!(err, TraceError::ForeignProvenance { .. }));

        let err = m
            .check_provenance(
                &WorldModelId::new("distributed", 2),
                &OracleId::new("properties+crash", 1),
            )
            .expect_err("a different model version must be refused");
        assert!(matches!(err, TraceError::ForeignProvenance { .. }));
    }

    #[test]
    fn a_multi_execution_bundle_is_refused_by_a_single_execution_reader() {
        let bytes = sample_bundle();
        let mut m = Bundle::parse(&bytes).expect("parse").manifest().expect("manifest");
        assert!(m.require_single_execution().is_ok());

        m.executions = 3;
        let err = m.require_single_execution().expect_err("must refuse");
        assert!(matches!(err, TraceError::MultipleExecutions { count: 3 }));
    }
    #[test]
    fn a_large_section_is_packed_and_comes_back_whole() {
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 7) as u8).collect();
        let mut w = BundleWriter::new();
        w.section(SectionId::Events, payload.clone());
        let bytes = w.finish([0; 16]);

        assert!(bytes.len() < payload.len(), "a compressible section was stored whole");
        let back = Bundle::parse(&bytes).expect("parse");
        assert_eq!(back.section(SectionId::Events), Some(&payload[..]));
    }

    #[test]
    fn an_unknown_section_flag_is_refused() {
        let mut bytes = sample_bundle();
        // Flags live at offset 4 of the first table entry.
        bytes[BUNDLE_HEADER_BYTES + 4] = 0x80;
        assert!(matches!(Bundle::parse(&bytes), Err(TraceError::UnknownSectionFlags { .. })));
    }

    #[test]
    fn a_foreign_tape_abi_is_refused_not_reinterpreted() {
        let mut manifest = manifest_for(ManifestInputs {
            reduced_from: None,
            state_digest: None,
            seed: Seed(1),
            trace_digest: Digest::of(b"x"),
            run_id: [0; 16],
            provenance: provenance(),
            outcome: Outcome::Clean,
            outcome_event: None,
            counters: Counters::default(),
            policy: PolicyRecord {
                scheduler: "random".to_owned(),
                pct_depth: None,
                switch_points: "syscall+futex".to_owned(),
            },
            degraded_reasons: Vec::new(),
            created_utc: now_utc(),
        });
        assert!(manifest.check_tape_abi(manifest.tape_abi).is_ok());
        manifest.tape_abi += 1;
        assert!(matches!(
            manifest.check_tape_abi(bohr_tape_abi()),
            Err(TraceError::TapeAbiMismatch { .. })
        ));
    }

    #[test]
    fn created_utc_follows_source_date_epoch() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
