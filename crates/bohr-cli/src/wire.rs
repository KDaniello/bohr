//! How a campaign hands work to a worker, and takes results back.
//!
//! Length-prefixed binary: a coverage map is 64 KB and a generation carries one per run.
//!
//! Not a stored format: the same binary reads it seconds later, so a mismatch is a defect. It
//! refuses damaged input, so a truncated result is never folded in as a run that covered nothing.

use bohr_explorer::{Origin, RunResult, WorkItem};
use bohr_tape::{DRAW_RECORD_BYTES, DrawRecord, Journal};
use bohr_types::{CoverageMap, Digest, Seed};

use crate::CliError;

/// What a file of work or results begins with, so a stale one is refused.
const MAGIC: &[u8; 8] = b"BOHRWIR2";

fn broken(what: &str) -> CliError {
    CliError::internal(format!("BOHR-3006: a worker handover was unreadable: {what}"))
}

/// A reader that refuses to run off the end.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, CliError> {
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(broken("it does not begin like one"));
        }
        Ok(Self { bytes, at: MAGIC.len() })
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CliError> {
        let end = self.at.checked_add(n).ok_or_else(|| broken("a length that does not fit"))?;
        if end > self.bytes.len() {
            return Err(broken("it ends in the middle of a record"));
        }
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, CliError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| broken("a short number"))?))
    }

    fn u64(&mut self) -> Result<u64, CliError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| broken("a short number"))?))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, CliError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn text(&mut self) -> Result<String, CliError> {
        String::from_utf8(self.bytes()?).map_err(|_| broken("text that is not text"))
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Encodes a generation's work.
#[must_use]
pub(crate) fn encode_work(items: &[WorkItem]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for item in items {
        out.extend_from_slice(&item.index.to_le_bytes());
        out.extend_from_slice(&item.seed.get().to_le_bytes());
        put_bytes(&mut out, &item.prefix);
    }
    out
}

/// Reads a generation's work back.
pub(crate) fn decode_work(bytes: &[u8]) -> Result<Vec<WorkItem>, CliError> {
    let mut cursor = Cursor::new(bytes)?;
    let mut items = Vec::new();
    while !cursor.done() {
        let index = cursor.u32()?;
        let seed = Seed(cursor.u64()?);
        let prefix = cursor.bytes()?;
        // The origin stays with the campaign; a worker only learns what to run.
        items.push(WorkItem { index, seed, prefix, origin: Origin::Seeded });
    }
    Ok(items)
}

/// What a file of results begins with, before any of them.
#[must_use]
pub(crate) fn results_header() -> Vec<u8> {
    MAGIC.to_vec()
}

/// Encodes one result, in the form a file of them is a sequence of.
///
/// One at a time, so a worker killed mid-generation has already reported the runs it finished.
#[must_use]
pub(crate) fn encode_result(result: &RunResult) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&result.index.to_le_bytes());
    out.extend_from_slice(result.digest.as_bytes());
    put_bytes(&mut out, result.outcome.as_deref().unwrap_or("").as_bytes());
    out.push(u8::from(result.outcome.is_some()));
    put_bytes(&mut out, result.coverage.as_bytes());
    out.extend_from_slice(&result.guest_edges.to_le_bytes());
    out.extend_from_slice(&result.adversity.to_le_bytes());
    out.extend_from_slice(&(result.observed.len() as u32).to_le_bytes());
    for (name, evaluations, held) in &result.observed {
        put_bytes(&mut out, name.as_bytes());
        out.extend_from_slice(&evaluations.to_le_bytes());
        out.extend_from_slice(&held.to_le_bytes());
    }
    out.extend_from_slice(&(result.journal.records().len() as u32).to_le_bytes());
    for record in result.journal.records() {
        out.extend_from_slice(&record.offset.to_le_bytes());
        out.extend_from_slice(&record.event.to_le_bytes());
        out.extend_from_slice(&record.value.to_le_bytes());
        out.extend_from_slice(&record.site.to_le_bytes());
        out.extend_from_slice(&record.pad.to_le_bytes());
        out.extend_from_slice(&record.len.to_le_bytes());
    }
    out
}

/// Encodes a whole file of results.
#[cfg(test)]
#[must_use]
pub(crate) fn encode_results(results: &[RunResult]) -> Vec<u8> {
    let mut out = results_header();
    for result in results {
        out.extend_from_slice(&encode_result(result));
    }
    out
}

/// Reads results back.
pub(crate) fn decode_results(bytes: &[u8]) -> Result<Vec<RunResult>, CliError> {
    let mut cursor = Cursor::new(bytes)?;
    let mut results = Vec::new();
    while !cursor.done() {
        let index = cursor.u32()?;
        let digest: [u8; 32] = cursor.take(32)?.try_into().map_err(|_| broken("a short digest"))?;
        let text = cursor.text()?;
        let present = cursor.take(1)?[0] != 0;
        let coverage = CoverageMap::from_bytes(&cursor.bytes()?)
            .ok_or_else(|| broken("a coverage map with no counters in it"))?;
        let guest_edges = cursor.u64()?;
        let adversity = cursor.u32()?;

        let stated = cursor.u32()? as usize;
        let mut observed = Vec::with_capacity(stated.min(1024));
        for _ in 0..stated {
            let name = cursor.text()?;
            observed.push((name, cursor.u64()?, cursor.u64()?));
        }

        let count = cursor.u32()? as usize;
        let mut records = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let raw = cursor.take(DRAW_RECORD_BYTES)?;
            let fixed = raw.try_into().map_err(|_| broken("a short draw record"))?;
            records
                .push(DrawRecord::from_bytes(fixed).map_err(|_| broken("a damaged draw record"))?);
        }
        let journal = Journal::from_records(records);

        results.push(RunResult {
            index,
            digest: Digest(digest),
            outcome: present.then_some(text),
            coverage,
            guest_edges,
            journal,
            adversity,
            observed,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use bohr_types::{EventId, TapeOffset};

    use super::*;

    fn a_result(index: u32) -> RunResult {
        let mut coverage = CoverageMap::new();
        let coverage_cells = coverage.cells();
        coverage.as_bytes_mut()[index as usize % coverage_cells] = 3;
        let mut journal = Journal::new();
        journal.push(TapeOffset(8), EventId(2), bohr_tape::DrawSite::SchedPick, 4, 0x1234);
        RunResult {
            index,
            digest: Digest::of(&index.to_le_bytes()),
            outcome: index.is_multiple_of(2).then(|| "guest crashed".to_owned()),
            coverage,
            guest_edges: 4096,
            journal,
            adversity: index,
            observed: vec![
                ("a leader was elected".to_owned(), 3, 2),
                ("a leader changed under a partition".to_owned(), 1, 0),
            ],
        }
    }

    #[test]
    fn work_survives_the_handover() {
        let items = vec![
            WorkItem { index: 0, seed: Seed(7), prefix: vec![1, 2, 3], origin: Origin::Seeded },
            WorkItem { index: 9, seed: Seed(0), prefix: Vec::new(), origin: Origin::Seeded },
        ];
        let back = decode_work(&encode_work(&items)).expect("what we wrote is readable");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].prefix, vec![1, 2, 3]);
        assert_eq!(back[1].index, 9);
        assert_eq!(back[0].seed.get(), 7);
    }

    #[test]
    fn results_survive_the_handover() {
        let results = vec![a_result(0), a_result(1)];
        let back = decode_results(&encode_results(&results)).expect("readable");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].outcome.as_deref(), Some("guest crashed"));
        assert_eq!(back[1].outcome, None, "a clean run must not come back as a finding");
        assert_eq!(back[0].coverage, results[0].coverage);
        assert_eq!(back[0].journal.records().len(), 1);
        assert_eq!(back[0].journal.records()[0].value, 0x1234);
    }

    /// Campaign-scoped properties survive the round trip; losing them would report every
    /// `sometimes` as never satisfied.
    #[test]
    fn what_the_guest_promised_survives_the_handover() {
        let back = decode_results(&encode_results(&[a_result(0)])).expect("readable");
        assert_eq!(
            back[0].observed,
            vec![
                ("a leader was elected".to_owned(), 3, 2),
                ("a leader changed under a partition".to_owned(), 1, 0),
            ],
            "the properties a worker observed did not come back"
        );
    }

    /// A file cut mid-record is refused. A file ending on a record boundary is a short list, which
    /// is valid; the campaign notices missing results.
    #[test]
    fn a_handover_cut_mid_record_is_refused_rather_than_half_read() {
        let whole = encode_results(&[a_result(0), a_result(1)]);
        let one = encode_results(&[a_result(0)]).len();

        for cut in [0, 1, 40, one / 2, one + 40, whole.len() - 1] {
            assert!(
                decode_results(&whole[..cut]).is_err(),
                "a handover cut at {cut} bytes was read as if it were whole"
            );
        }
        assert_eq!(
            decode_results(&whole[..one]).expect("a record boundary is a short list").len(),
            1,
            "a worker that wrote one of two results wrote one result"
        );
        assert_eq!(decode_results(&whole).expect("the whole thing").len(), 2);
    }

    #[test]
    fn something_that_is_not_a_handover_is_refused() {
        assert!(decode_work(b"not ours at all").is_err());
        assert!(decode_results(&[]).is_err());
    }
}
