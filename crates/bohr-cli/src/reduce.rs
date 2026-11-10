//! `bohr reduce`: the smallest scenario that still goes wrong.
//!
//! The decisions are all in `bohr-reducer`, which cannot run anything. What is here is the other
//! half: carry out what it asks for, tell it what happened, and write the answer as a bundle
//! somebody can replay. The division is the same one the search makes, and for the same reason —
//! every claim about what a reduction guarantees is decided in the crate with no processes in it.

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
            return Err(CliError::Usage(format!(
                "BOHR-1001: no such thing to keep as {other:?}; this build minimises \
                 events, tape or both"
            )));
        }
    };

    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let bundle = Bundle::parse(&bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    let manifest = bundle.manifest().map_err(|e| CliError::Usage(e.to_string()))?;
    manifest.check_tape_abi(TAPE_ABI).map_err(|e| CliError::Usage(e.to_string()))?;
    // A reduction replays the bundle hundreds of times and asks whether each one still reproduces
    // the same violation, so it is judged by whatever this build wires — and a build wiring a
    // different oracle would find the baseline itself clean and report that the bundle reproduces
    // nothing. That message points a reader at the bundle; this one points at the mismatch.
    let stored_config = bundle
        .section(SectionId::Config)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let stored = optional_config(&stored_config)?;
    let (world, oracle) = crate::identities(stored.as_ref());
    manifest.check_provenance(&world, &oracle).map_err(|e| CliError::Usage(e.to_string()))?;

    let tape_bytes = bundle
        .section(SectionId::Tape)
        .ok_or_else(|| CliError::Usage("BOHR-4002: bundle is missing required section 2".into()))?;
    let tape = Tape::from_container(tape_bytes).map_err(|e| CliError::Usage(e.to_string()))?;
    // The prefix is written out to cover every byte the run read, once, before anything is
    // proposed. The tape reads its keystream at an offset counted from the end of the prefix, so a
    // prefix of a different length is a different tape — fixing it here is what makes every later
    // edit an overwrite of one decision rather than a shift of all of them.
    //
    // How far the run read is read off its own journal rather than guessed at: a bound short of it
    // does not leave the decisions past it alone, it moves every one of them.
    let reach =
        bundle.section(SectionId::DrawJournal).map(reach_of).transpose()?.unwrap_or_default();
    let start = Scenario::new(tape.seed(), materialise(&tape, reach, path)?, stored_config);

    let carry_out =
        |scenario: &Scenario| -> Result<(Option<Violation>, Size, bohr_tape::Journal), CliError> {
            let tape = Tape::with_prefix(scenario.seed(), scenario.prefix().to_vec());
            let loaded = if scenario.config().trim().is_empty() {
                None
            } else {
                match bohr_world::config::load(scenario.config()) {
                    Ok(loaded) => Some(loaded),
                    // **A candidate the world refuses did not reproduce; it did not kill the
                    // reduction.** Minimisation over the entries will propose a subset with no
                    // `[[partition]]` left, and a scenario that watches for a heal refuses one --
                    // so every liveness finding died part-way through its own reduction, with the
                    // budget already spent and the *user's* file reported as a usage error.
                    // The size is never read for a candidate that did not reproduce; nought
                    // says a run never happened rather than that one happened and was empty.
                    Err(_) => {
                        let nothing = Size { events: 0, draws: 0 };
                        return Ok((None, nothing, bohr_tape::Journal::new()));
                    }
                }
            };
            let target = replay::target_from(&bundle, &manifest, loaded.as_ref(), binaries)?;
            let completed = execute::once(execute::Execution {
                target: &target,
                tape: &tape,
                config: crate::run_config_for(loaded.as_ref(), None),
                output: execute::Output::Discard,
                policy: &manifest.policy.scheduler,
                record: execute::Record::Log,
                scenario: loaded.as_ref(),
                config_toml: scenario.config(),
            })?;
            let (events, _) = bohr_trace::read_events_lossy(&completed.log);
            let violation =
                Violation::of(&completed.report.outcome, completed.report.stop_reason, &events);
            let size = Size {
                events: completed.report.events,
                draws: completed.report.journal.len() as u64,
            };
            Ok((violation, size, completed.report.journal))
        };

    let (violation, size, journal) = carry_out(&start)?;
    let Some(violation) = violation else {
        return Err(CliError::Usage(format!(
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
    let mut reduction = Reduction::new(start, baseline, Limits { replays: budget, keep })
        .map_err(|e| CliError::Usage(e.to_string()))?;

    while let Some(candidate) = reduction.next() {
        let id = candidate.id;
        let (violation, size, journal) = carry_out(&candidate.scenario)?;
        reduction.accept(&CandidateOutcome { id, violation, size, journal });
        // Carriage-returned so a long reduction is one line rather than a thousand. On the error
        // stream, because everything the tool says about a run goes there.
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
    // The reduced scenario's own bounds, not the original's: reduction may have removed the
    // section that carried them, and the bundle written here has to replay to what this run does.
    let reduced = optional_config(best.config())?;
    let completed = execute::once(execute::Execution {
        target: &replay::target_from(&bundle, &manifest, reduced.as_ref(), binaries)?,
        tape: &tape,
        config: crate::run_config_for(reduced.as_ref(), None),
        output: execute::Output::Discard,
        policy: &manifest.policy.scheduler,
        record: execute::Record::Log,
        scenario: reduced.as_ref(),
        config_toml: best.config(),
    })?;
    let written = execute::to_bundle(&completed, &tape, Some(manifest.run_id.clone()))?;
    std::fs::write(out, written)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot write {out}: {e}")))?;

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
/// `shrank` is false where neither events nor draws went down: the bundle written is then the
/// finding as it was, re-recorded, and a reader acting on "reduced" should know it is not smaller.
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
    if text.trim().is_empty() { Ok(None) } else { Ok(Some(bohr_world::config::load(text)?)) }
}

/// Writes the tape's first bytes down so every decision the run made sits inside the prefix.
///
/// Read from the tape rather than from the journal: the container carries only the prefix, and
/// everything past it came from the keystream — which is where most of a long run's decisions are
/// drawn from and none of them can be edited. Materialising is what brings them within reach.
fn materialise(tape: &Tape, reach: u64, path: &str) -> Result<Vec<u8>, CliError> {
    /// How far a reduction is prepared to write the keystream out.
    ///
    /// A bound rather than a preference: the tape is infinite, and a run that read a gigabyte would
    /// otherwise be given a gigabyte of prefix to carry through every candidate. Past it a
    /// reduction is refused rather than attempted, for the reason below.
    const MOST: u64 = 64 * 1024 * 1024;

    // **Short of what the run read is not a smaller prefix, it is a different tape.** The extension
    // keystream is counted from the end of the prefix (`docs/API.md` §2.3), so the byte a run saw
    // at offset X past a prefix of length P is the keystream's byte at X − P. Write out fewer bytes
    // than the run read and every decision after them moves: what replays is a different run, and a
    // reduction that started there would minimise something that never happened.
    //
    // This was a bound of 128 KiB and a claim that decisions past it were left alone. A run of an
    // execution client reads several megabytes, so its bundle replayed clean and its finding could
    // not be reduced at all.
    let wanted = (tape.prefix().len() as u64).max(reach);
    if wanted > MOST {
        return Err(CliError::Usage(format!(
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
/// A trailing part-record is ignored rather than refused: what is wanted is a bound, and a journal
/// that lost its last bytes still says how far the rest of the run reached.
fn reach_of(section: &[u8]) -> Result<u64, CliError> {
    Ok(section
        .as_chunks::<{ bohr_tape::DRAW_RECORD_BYTES }>()
        .0
        .iter()
        .filter_map(|bytes| {
            // Damage is skipped rather than fatal, for the reason above: how far the readable part
            // of a journal reached is still an answer.
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

    /// The property a reduction rests on: the tape it starts from serves the bytes the run saw.
    ///
    /// A prefix is not a window onto the same tape. The extension keystream is counted from the end
    /// of the prefix, so a byte past a short prefix is a *different* byte — and a reduction that
    /// began there would minimise a run that never happened. It did: the bound was 128 KiB, an
    /// execution client reads several megabytes, and its findings replayed clean and were refused.
    #[test]
    fn the_prefix_serves_every_byte_the_run_read() {
        const READ: u64 = 300_000;

        let original = Tape::from_seed(Seed(7));
        let prefix = materialise(&original, READ, "test").expect("well inside the bound");
        let reduced = Tape::with_prefix(Seed(7), prefix);

        // Sampled across the reach rather than compared whole, because what fails is an offset past
        // the old bound and a sample that stopped short of one would pass either way.
        for at in [0, 1_000, 131_071, 131_072, 200_000, READ - 8] {
            let (mut was, mut now) = ([0u8; 8], [0u8; 8]);
            original.read_at(at, &mut was);
            reduced.read_at(at, &mut now);
            assert_eq!(was, now, "the tapes differ at offset {at}");
        }
    }

    /// The object says what went in and what came out, and whether it is any smaller: a reduction
    /// that removed nothing still writes a bundle, and `shrank` is how a reader tells.
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

    /// And a run that read more than a reduction will write out is refused rather than reduced,
    /// because reducing it would minimise a different run.
    #[test]
    fn a_run_too_long_to_write_out_is_refused_and_says_why() {
        let tape = Tape::from_seed(Seed(7));
        let refused = materialise(&tape, 512 * 1024 * 1024, "big.bohr");
        let Err(CliError::Usage(said)) = refused else {
            panic!("a run past the bound was not refused");
        };
        assert!(said.contains("BOHR-3006"), "{said}");
        assert!(said.contains("different run"), "the refusal must say what it is avoiding: {said}");
    }
}
