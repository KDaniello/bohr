//! A campaign at the corpus's capacity is written down and read back in well under two seconds,
//! so resuming costs nothing a campaign notices.
//!
//! A measurement, ignored by default: `cargo test -p bohr-cli --release --test resume_speed --
//! --ignored --nocapture` prints what the gate records. It lives here because the explorer may not
//! read a clock.

use bohr_backend::CoverageMap;
use bohr_explorer::{Campaign, RunResult};
use bohr_tape::Journal;
use bohr_types::{Digest, EventId, Seed, TapeOffset};

fn result(index: u32, cell: usize) -> RunResult {
    let mut coverage = CoverageMap::new();
    let cells = coverage.cells();
    coverage.as_bytes_mut()[cell % cells] = 1;
    let mut journal = Journal::new();
    journal.push(TapeOffset(0), EventId(1), bohr_tape::DrawSite::SchedPick, 4, 7);
    RunResult {
        index,
        digest: Digest([0; 32]),
        outcome: None,
        coverage,
        guest_edges: 10,
        journal,
        adversity: 0,
        observed: Vec::new(),
    }
}

#[test]
#[ignore = "a measurement, run by hand"]
#[allow(clippy::disallowed_methods)]
fn a_full_corpus_resumes_quickly() {
    let mut campaign = Campaign::new(Seed(9));
    let mut cell = 0usize;
    while campaign.corpus().len() < bohr_explorer::DEFAULT_CAPACITY {
        let planned = campaign.plan(32);
        let results = planned
            .iter()
            .map(|item| {
                cell += 1;
                result(item.index, cell)
            })
            .collect();
        campaign.fold(&planned, results);
    }
    let started = std::time::Instant::now();
    let state = campaign.to_state();
    let written = started.elapsed();
    let started = std::time::Instant::now();
    let back = Campaign::from_state(&state).expect("it reads back");
    let read = started.elapsed();
    println!(
        "{} entries, {} bytes, written in {written:?}, read in {read:?}",
        back.corpus().len(),
        state.len()
    );
    assert!(read < std::time::Duration::from_secs(2));
}
