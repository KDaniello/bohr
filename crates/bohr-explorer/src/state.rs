//! A campaign written down between generations, so it can be resumed as the same campaign.
//!
//! **Not a snapshot of tapes but of the whole search.** A corpus directory keeps tapes and has to
//! re-run them to learn what they covered, which is a different campaign from the one that stopped:
//! visit counts, evictions, credit and findings are gone. This keeps all of it, and a campaign
//! resumed from it plans the same next generation an uninterrupted one would.
//!
//! The format is versioned, and a build reads only its own version: a state of another is refused
//! with both numbers, never misread as a smaller campaign.

use bohr_backend::CoverageMap;
use bohr_tape::{DRAW_RECORD_BYTES, DrawRecord, Journal};
use bohr_types::{Digest, Seed};

use crate::{
    Campaign, Finding, Operator, Origin, Tally,
    corpus::{Corpus, Entry, EntryId},
};

/// What a state begins with, and the version of everything after it.
const MAGIC: &[u8; 8] = b"BOHRCAMP";
const VERSION: u32 = 1;

/// Why a state was not read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StateError(pub String);

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every operator, in the order its number is its position. Appended to, never reordered.
const OPERATORS: [Operator; 8] = [
    Operator::BitFlip,
    Operator::ByteFlip,
    Operator::Arithmetic,
    Operator::Interesting,
    Operator::Splice,
    Operator::Extend,
    Operator::SiteBurst,
    Operator::Havoc,
];

fn operator_number(operator: Operator) -> u8 {
    OPERATORS.iter().position(|o| *o == operator).map_or(u8::MAX, |n| n as u8)
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
    }
    fn digest(&mut self, v: &Digest) {
        self.0.extend_from_slice(v.as_bytes());
    }
    fn journal(&mut self, journal: &Journal) {
        self.u32(journal.records().len() as u32);
        for record in journal.records() {
            self.0.extend_from_slice(&record.to_bytes());
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

fn broken(what: &str) -> StateError {
    StateError(format!("the campaign state is unreadable: {what}"))
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], StateError> {
        let end = self.at.checked_add(n).ok_or_else(|| broken("a length that does not fit"))?;
        let out = self.bytes.get(self.at..end).ok_or_else(|| broken("it ends early"))?;
        self.at = end;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, StateError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, StateError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| broken("a short number"))?))
    }
    fn u64(&mut self) -> Result<u64, StateError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| broken("a short number"))?))
    }
    fn bytes(&mut self) -> Result<Vec<u8>, StateError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
    fn text(&mut self) -> Result<String, StateError> {
        String::from_utf8(self.bytes()?).map_err(|_| broken("text that is not text"))
    }
    fn digest(&mut self) -> Result<Digest, StateError> {
        Ok(Digest(self.take(32)?.try_into().map_err(|_| broken("a short digest"))?))
    }
    fn journal(&mut self) -> Result<Journal, StateError> {
        let count = self.u32()? as usize;
        let mut records = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let raw: [u8; DRAW_RECORD_BYTES] =
                self.take(DRAW_RECORD_BYTES)?.try_into().map_err(|_| broken("a short draw"))?;
            records.push(DrawRecord::from_bytes(&raw).map_err(|_| broken("a damaged draw"))?);
        }
        Ok(Journal::from_records(records))
    }
}

impl Campaign {
    /// The whole campaign as bytes: corpus, coverage, findings, credit, properties and counters.
    #[must_use]
    pub fn to_state(&self) -> Vec<u8> {
        let mut w = Writer(MAGIC.to_vec());
        w.u32(VERSION);
        for v in [self.master.get(), self.generation, self.runs, self.adversity, self.guest_edges] {
            w.u64(v);
        }
        let corpus = &self.corpus;
        w.u64(corpus.capacity as u64);
        w.u64(corpus.next_id);
        w.u64(corpus.evicted);
        w.bytes(corpus.seen.as_bytes());
        w.u32(corpus.entries.len() as u32);
        for entry in &corpus.entries {
            w.u64(entry.id.0);
            w.u64(entry.seed.get());
            w.bytes(&entry.prefix);
            match entry.origin {
                Origin::Seeded => w.u8(0),
                Origin::Mutated { parent, operator } => {
                    w.u8(1);
                    w.u64(parent.0);
                    w.u8(operator_number(operator));
                }
            }
            w.u64(entry.visits);
            w.u64(entry.edges as u64);
            w.journal(&entry.draws);
        }
        w.u32(self.findings.len() as u32);
        for finding in self.findings.values() {
            w.digest(&finding.digest);
            w.bytes(finding.kind.as_bytes());
            w.u64(finding.seed.get());
            w.bytes(&finding.prefix);
            w.u64(finding.generation);
        }
        w.u32(self.credit.len() as u32);
        for (operator, count) in &self.credit {
            w.u8(operator_number(*operator));
            w.u64(*count);
        }
        w.u64(self.properties.refused);
        w.u32(self.properties.by_name.len() as u32);
        for (name, tally) in &self.properties.by_name {
            w.bytes(name.as_bytes());
            w.u64(tally.evaluations);
            w.u64(tally.satisfied);
            match &tally.first_satisfied {
                Some(digest) => {
                    w.u8(1);
                    w.digest(digest);
                }
                None => w.u8(0),
            }
        }
        w.0
    }

    /// The campaign a state describes, or why it does not describe one.
    pub fn from_state(bytes: &[u8]) -> Result<Self, StateError> {
        if bytes.get(..MAGIC.len()) != Some(MAGIC.as_slice()) {
            return Err(broken("it does not begin like one"));
        }
        let mut r = Reader { bytes, at: MAGIC.len() };
        let version = r.u32()?;
        if version != VERSION {
            return Err(StateError(format!(
                "the campaign state is version {version} and this build reads {VERSION}"
            )));
        }
        let operator = |n: u8| {
            OPERATORS.get(n as usize).copied().ok_or_else(|| broken("an operator nobody defined"))
        };
        let mut campaign = Self::new(Seed(r.u64()?));
        campaign.generation = r.u64()?;
        campaign.runs = r.u64()?;
        campaign.adversity = r.u64()?;
        campaign.guest_edges = r.u64()?;

        let mut corpus = Corpus::with_capacity(r.u64()? as usize);
        corpus.next_id = r.u64()?;
        corpus.evicted = r.u64()?;
        corpus.seen =
            CoverageMap::from_bytes(&r.bytes()?).ok_or_else(|| broken("an empty coverage map"))?;
        for _ in 0..r.u32()? {
            let id = EntryId(r.u64()?);
            let seed = Seed(r.u64()?);
            let prefix = r.bytes()?;
            let origin = match r.u8()? {
                0 => Origin::Seeded,
                1 => Origin::Mutated { parent: EntryId(r.u64()?), operator: operator(r.u8()?)? },
                _ => return Err(broken("an origin nobody defined")),
            };
            let visits = r.u64()?;
            let edges = r.u64()? as usize;
            let draws = r.journal()?;
            corpus.entries.push(Entry { id, seed, prefix, origin, visits, edges, draws });
        }
        campaign.corpus = corpus;

        for _ in 0..r.u32()? {
            let digest = r.digest()?;
            let kind = r.text()?;
            let seed = Seed(r.u64()?);
            let prefix = r.bytes()?;
            let generation = r.u64()?;
            campaign.findings.insert(digest, Finding { digest, kind, seed, prefix, generation });
        }
        for _ in 0..r.u32()? {
            let op = operator(r.u8()?)?;
            campaign.credit.insert(op, r.u64()?);
        }
        campaign.properties.refused = r.u64()?;
        for _ in 0..r.u32()? {
            let name = r.text()?;
            let evaluations = r.u64()?;
            let satisfied = r.u64()?;
            let first_satisfied = if r.u8()? == 1 { Some(r.digest()?) } else { None };
            campaign
                .properties
                .by_name
                .insert(name, Tally { evaluations, satisfied, first_satisfied });
        }
        if r.at != bytes.len() {
            return Err(broken("it carries more than a campaign"));
        }
        Ok(campaign)
    }
}

#[cfg(test)]
mod tests {
    use bohr_types::{EventId, TapeOffset};

    use super::*;
    use crate::RunResult;

    fn result(index: u32, cell: usize, finding: bool) -> RunResult {
        let mut coverage = CoverageMap::new();
        let cells = coverage.cells();
        coverage.as_bytes_mut()[cell % cells] = 1;
        let mut journal = Journal::new();
        journal.push(TapeOffset(0), EventId(1), bohr_tape::DrawSite::SchedPick, 4, 7);
        RunResult {
            index,
            digest: Digest([index as u8; 32]),
            outcome: finding.then(|| "GuestCrashed".to_owned()),
            coverage,
            guest_edges: 10,
            journal,
            adversity: 1,
            observed: vec![("always holds".to_owned(), true)],
        }
    }

    /// A campaign written down and read back plans the same next generation and reports the same.
    #[test]
    fn a_state_reads_back_as_the_same_campaign() {
        let mut campaign = Campaign::new(Seed(42));
        for generation in 0..3u32 {
            let planned = campaign.plan(8);
            let results = planned
                .iter()
                .map(|item| {
                    result(item.index, (generation * 8 + item.index) as usize, item.index == 3)
                })
                .collect();
            campaign.fold(&planned, results);
        }
        let back = Campaign::from_state(&campaign.to_state()).expect("it reads back");

        assert_eq!(back.to_state(), campaign.to_state(), "byte for byte");
        let (a, b) = (campaign.plan(8), back.plan(8));
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!((x.seed, &x.prefix), (y.seed, &y.prefix));
        }
        assert_eq!(back.findings().len(), campaign.findings().len());
        assert_eq!(back.runs(), 24);
    }

    /// Damage is refused, never read as a smaller campaign.
    #[test]
    fn a_damaged_state_is_refused() {
        let state = Campaign::new(Seed(1)).to_state();
        assert!(Campaign::from_state(&state[..state.len() - 1]).is_err());
        assert!(Campaign::from_state(b"BOHRWIRE").is_err());
        let mut later = state.clone();
        later[8] = 2;
        assert!(Campaign::from_state(&later).unwrap_err().0.contains("version 2"));
    }
}
