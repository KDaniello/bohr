//! `bohr reduce`: the smallest scenario that still goes wrong.
//!
//! The decisions live in `bohr-reducer`, which runs nothing. This module runs what it proposes,
//! reports the results back, and writes the answer as a replayable bundle.

use bohr_reducer::{
    Baseline, CandidateOutcome, Keep, Limits, Reduction, Scenario, Size, Violation,
};
use bohr_tape::{TAPE_ABI, Tape};
use bohr_trace::{Bundle, SectionId};

use crate::{CliError, execute, replay};

/// Reduces a stored finding and writes what is left.
pub(crate) fn reduce(
    path: &str,
    out: &str,
    budget: u32,
    keep: &str,
    binaries: Option<&str>,
) -> Result<u8, CliError> {
    let keep = match keep {
        "events" => Keep::Events,
        "tape" => Keep::Tape,
        "both" => Keep::Both,
        other => {
            return Err(CliError::usage(format!(
                "BOHR-1001: no such thing to keep as {other:?}; this build minimises \
                 events, tape or both"
            )));
        }
    };

    let bytes = std::fs::read(path)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let bundle = Bundle::parse(&bytes).map_err(|e| CliError::usage(e.to_string()))?;
    let manifest = bundle.manifest().map_err(|e| CliError::usage(e.to_string()))?;
    manifest.check_tape_abi(TAPE_ABI).map_err(|e| CliError::usage(e.to_string()))?;
    // Every candidate runs the recorded programs, so a changed one is refused before the first.
    replay::verify_binaries(&bundle, &manifest, binaries)?;
    // Candidates are judged by this build's oracle. A bundle recorded under another one is refused
    // with that reason, instead of reporting that the baseline reproduces nothing.
    let stored_config = bundle
        .config_text()
        .map_err(|e| CliError::usage(format!("BOHR-4002: config section: {e}")))?
        .to_owned();
    let stored = optional_config(&stored_config)?;
    replay::verify_state(&manifest, stored.as_ref())?;
    let (world, oracle) = crate::identities(stored.as_ref());
    manifest.check_provenance(&world, &oracle).map_err(|e| CliError::usage(e.to_string()))?;

    let tape_bytes = bundle
        .section(SectionId::Tape)
        .ok_or_else(|| CliError::usage("BOHR-4002: bundle is missing required section 2"))?;
    let tape = Tape::from_container(tape_bytes).map_err(|e| CliError::usage(e.to_string()))?;
    // The prefix is written out once to cover every byte the run read, as its journal records.
    // The keystream is counted from the end of the prefix, so a fixed prefix length makes each
    // later edit change one decision instead of shifting all of them.
    let reach =
        bundle.section(SectionId::DrawJournal).map(reach_of).transpose()?.unwrap_or_default();
    let start = Scenario::new(tape.seed(), materialise(&tape, reach, path)?, stored_config);

    // What a candidate came to: its violation, its size, its draws, and the digest of its run.
    type Carried = (Option<Violation>, Size, bohr_tape::Journal, bohr_types::Digest);
    let carry_out = |scenario: &Scenario| -> Result<Carried, CliError> {
        let tape = Tape::with_prefix(scenario.seed(), scenario.prefix().to_vec());
        let loaded = if scenario.config().trim().is_empty() {
            None
        } else {
            match crate::state_dir::load(scenario.config()) {
                Ok(loaded) => Some(loaded),
                // A candidate the world refuses (for example a liveness scenario left without
                // its `[[partition]]`) counts as not reproduced. Its size is never read.
                Err(_) => {
                    let nothing = Size { events: 0, draws: 0 };
                    let none = bohr_types::Digest::of(&[]);
                    return Ok((None, nothing, bohr_tape::Journal::new(), none));
                }
            }
        };
        let target = replay::target_from(&bundle, &manifest, loaded.as_ref(), binaries)?;
        let completed = execute::once(execute::Execution {
            target: &target,
            tape: &tape,
            config: {
                let mut config = crate::run_config_for(loaded.as_ref(), None);
                config.max_events = replay::budget_for(&manifest, config.max_events);
                config
            },
            output: execute::Output::Discard,
            policy: &manifest.policy.scheduler,
            // Spooled and read for the events a violation and its place are told by, so a
            // candidate holds none of its log.
            record: execute::Record::Spool,
            scenario: loaded.as_ref(),
            config_toml: scenario.config(),
        })?;
        let events = match completed.spooled.as_ref() {
            Some(spooled) => {
                spooled
                    .events_where(bohr_findings::group::telling)
                    .map_err(|e| {
                        CliError::internal(format!("a candidate's spooled log is unreadable: {e}"))
                    })?
                    .0
            }
            None => Vec::new(),
        };
        // Where it fell is part of what it is: a candidate that falls elsewhere is another
        // finding, as `bohr findings` groups them.
        let place = bohr_findings::group::place_of(&events, &completed.output_tails);
        let violation =
            Violation::of(&completed.report.outcome, completed.report.stop_reason, &events)
                .map(|violation| violation.at(place));
        let size =
            Size { events: completed.report.events, draws: completed.report.journal.len() as u64 };
        Ok((violation, size, completed.report.journal, completed.report.digest))
    };

    // The baseline is the stored run itself; one that goes elsewhere under this build would
    // reduce another finding under this one's name.
    let (violation, size, journal, digest) = carry_out(&start)?;
    if digest.to_string() != manifest.trace_digest {
        return Err(CliError::internal(format!(
            "BOHR-2011: {path} did not reproduce its own finding under this build: it ran to {digest} \
             where it recorded {}. bohr replay reads the same disagreement.",
            manifest.trace_digest
        )));
    }
    let Some(violation) = violation else {
        return Err(CliError::usage(format!(
            "BOHR-5003: {path} replayed clean, so there is nothing to reduce. A bundle reduces \
             what went wrong in it, and this one records nothing going wrong."
        )));
    };
    eprintln!(
        "reducing {} events · {}",
        size.events,
        bohr_report::describe::outcome_word(&manifest.outcome)
    );

    let baseline = Baseline { violation, size, journal };
    let before = size;
    let mut reduction = Reduction::new(
        start,
        baseline,
        Limits { replays: budget, keep, moments: &bohr_world::config::MOMENT_KEYS },
    )
    .map_err(|e| CliError::usage(e.to_string()))?;

    while let Some(candidate) = reduction.next() {
        let id = candidate.id;
        let (violation, size, journal, _) = carry_out(&candidate.scenario)?;
        reduction.accept(&CandidateOutcome { id, violation, size, journal });
        // One redrawn line on stderr.
        eprint!(
            "\rreduce  {} · {} of {budget} replays · events {} · draws {}   ",
            pass_word(reduction.pass()),
            reduction.spent().total(),
            reduction.size().events,
            reduction.size().draws
        );
    }
    eprintln!();

    let after = reduction.size();
    let best = reduction.best();
    let tape = Tape::with_prefix(best.seed(), best.prefix().to_vec());
    // The reduced scenario's bounds, which its bundle replays under.
    let reduced = optional_config(best.config())?;
    let completed = execute::once(execute::Execution {
        target: &replay::target_from(&bundle, &manifest, reduced.as_ref(), binaries)?,
        tape: &tape,
        // The budget its candidates ran under, or the finding it reduced may end elsewhere.
        config: {
            let mut config = crate::run_config_for(reduced.as_ref(), None);
            config.max_events = replay::budget_for(&manifest, config.max_events);
            config
        },
        output: execute::Output::Discard,
        policy: &manifest.policy.scheduler,
        record: execute::Record::Log,
        scenario: reduced.as_ref(),
        config_toml: best.config(),
    })?;
    let written = execute::to_bundle(&completed, &tape, Some(manifest.run_id.clone()))?;
    std::fs::write(out, written)
        .map_err(|e| CliError::usage(format!("BOHR-1002: cannot write {out}: {e}")))?;

    let spent = reduction.spent();
    if crate::json::on() {
        let verdict = bohr_report::narrate::verdict(&manifest.outcome);
        crate::json::emit(
            "reduce",
            account(path, out, &manifest.run_id, &verdict, (before, after), spent, budget),
        );
    } else {
        println!(
            "events: {} → {}, tape: {} → {} draws",
            before.events, after.events, before.draws, after.draws
        );
    }
    eprintln!(
        "  replays spent: {} verifying, {} on faults, {} on the configuration, {} on deliveries, \
         {} on the tape · written to {out}, reduced from run {}",
        spent.verifying, spent.faults, spent.config, spent.events, spent.tape, manifest.run_id
    );
    Ok(crate::exit::CLEAN)
}

/// A reduction as the body of a `bohr.reduce` object: the sizes before and after, the replays
/// each pass spent, and the bundle written.
///
/// `shrank` is false when neither events nor draws went down; the bundle is then the original
/// finding, re-recorded.
fn account(
    from: &str,
    out: &str,
    run_id: &str,
    verdict: &bohr_report::narrate::Verdict,
    (before, after): (Size, Size),
    spent: bohr_reducer::Spent,
    budget: u32,
) -> serde_json::Value {
    serde_json::json!({
        "bundle": from,
        "out": out,
        "from_run": run_id,
        "outcome": verdict.code,
        "headline": verdict.headline,
        "events": { "before": before.events, "after": after.events },
        "draws": { "before": before.draws, "after": after.draws },
        "shrank": after.events < before.events || after.draws < before.draws,
        "budget": budget,
        "replays_spent": {
            "verifying": spent.verifying,
            "faults": spent.faults,
            "config": spent.config,
            "events": spent.events,
            "tape": spent.tape,
            "total": spent.total(),
        },
    })
}

/// The configuration, where there is one.
fn optional_config(text: &str) -> Result<Option<bohr_world::LoadedConfig>, CliError> {
    if text.trim().is_empty() { Ok(None) } else { Ok(Some(crate::state_dir::load(text)?)) }
}

/// Writes the tape's first bytes down so every decision the run made sits inside the prefix.
///
/// Decisions past the stored prefix come from the keystream and cannot be edited; writing them out
/// makes them editable.
fn materialise(tape: &Tape, reach: u64, path: &str) -> Result<Vec<u8>, CliError> {
    /// How far a reduction is prepared to write the keystream out.
    ///
    /// A run that read more is refused, since every candidate would carry that much prefix.
    const MOST: u64 = 64 * 1024 * 1024;

    // A shorter prefix is a different tape: the keystream is counted from the prefix's end
    // (`docs/API.md` §2.3), so byte X past a prefix of length P is keystream byte X − P. Writing
    // out less than the run read would shift every later decision.
    let wanted = (tape.prefix().len() as u64).max(reach);
    if wanted > MOST {
        return Err(CliError::usage(format!(
            "BOHR-3006: {path} read {wanted} bytes of tape and a reduction writes out at most \
             {MOST}. Reducing it would minimise a different run, so it is refused instead."
        )));
    }
    let mut bytes = vec![0u8; wanted as usize];
    tape.read_at(0, &mut bytes);
    Ok(bytes)
}

/// How far into the tape a recorded run actually read, from its own journal.
///
/// A truncated last record is ignored; the rest still gives the bound.
fn reach_of(section: &[u8]) -> Result<u64, CliError> {
    Ok(section
        .as_chunks::<{ bohr_tape::DRAW_RECORD_BYTES }>()
        .0
        .iter()
        .filter_map(|bytes| {
            // Damage is skipped; the readable part still gives the bound.
            let record = bohr_tape::DrawRecord::from_bytes(bytes).ok()?;
            Some(record.offset.saturating_add(u64::from(record.len)))
        })
        .max()
        .unwrap_or_default())
}

/// What a reduction pass is doing, in words.
const fn pass_word(pass: bohr_reducer::Pass) -> &'static str {
    match pass {
        bohr_reducer::Pass::Faults => "dropping and moving faults",
        bohr_reducer::Pass::Config => "dropping scheduled faults",
        bohr_reducer::Pass::Events => "dropping deliveries",
        bohr_reducer::Pass::Tape => "plainer draws",
        bohr_reducer::Pass::Done => "done",
    }
}

#[cfg(test)]
mod tests {
    use bohr_types::Seed;

    use super::*;

    /// The materialised tape serves the same bytes the run saw, well past 128 KiB.
    #[test]
    fn the_prefix_serves_every_byte_the_run_read() {
        const READ: u64 = 300_000;

        let original = Tape::from_seed(Seed(7));
        let prefix = materialise(&original, READ, "test").expect("well inside the bound");
        let reduced = Tape::with_prefix(Seed(7), prefix);

        // Sampled across the whole reach, including offsets past 128 KiB.
        for at in [0, 1_000, 131_071, 131_072, 200_000, READ - 8] {
            let (mut was, mut now) = ([0u8; 8], [0u8; 8]);
            original.read_at(at, &mut was);
            reduced.read_at(at, &mut now);
            assert_eq!(was, now, "the tapes differ at offset {at}");
        }
    }

    /// The object gives sizes before and after; `shrank` is false when nothing was removed.
    #[test]
    fn a_reduction_object_carries_both_sizes_and_the_replays() {
        let verdict = bohr_report::narrate::verdict(&bohr_trace::Outcome::GuestCrashed(
            "node 0 exited 1".to_owned(),
        ));
        let spent = bohr_reducer::Spent { verifying: 2, faults: 3, config: 0, events: 5, tape: 7 };
        let before = Size { events: 900, draws: 40 };
        let after = Size { events: 120, draws: 40 };
        let value = crate::json::object(
            "reduce",
            account("in.bohr", "out.bohr", "run-1", &verdict, (before, after), spent, 500),
        );
        assert_eq!(value["schema"], "bohr.reduce/1");
        assert_eq!(value["bundle"], "in.bohr");
        assert_eq!(value["out"], "out.bohr");
        assert_eq!(value["from_run"], "run-1");
        assert_eq!(value["events"]["before"], 900);
        assert_eq!(value["events"]["after"], 120);
        assert_eq!(value["draws"]["after"], 40);
        assert_eq!(value["shrank"], true);
        assert_eq!(value["replays_spent"]["total"], 17);
        let same = account("a", "b", "r", &verdict, (before, before), spent, 500);
        assert_eq!(same["shrank"], false);
    }

    /// A run that read past the limit is refused.
    #[test]
    fn a_run_too_long_to_write_out_is_refused_and_says_why() {
        let tape = Tape::from_seed(Seed(7));
        let refused = materialise(&tape, 512 * 1024 * 1024, "big.bohr");
        let Err(refusal) = refused else {
            panic!("a run past the bound was not refused");
        };
        let said = refusal.to_string();
        assert_eq!(refusal.code, Some(bohr_types::Code(3006)), "{said}");
        assert!(said.contains("different run"), "the refusal must say what it is avoiding: {said}");
    }
}
