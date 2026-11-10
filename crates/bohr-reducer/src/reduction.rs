//! A reduction in progress: what has been proven smaller, and what to try next.

use std::collections::BTreeSet;

use bohr_tape::Journal;

use crate::{
    ReducerError,
    bytes::{Target, decisions, fired_at, fired_node_faults, movable, sooner},
    config::ConfigDoc,
    ddmin::DdMin,
    scenario::{Scenario, Size},
    violation::Violation,
};

/// What a reduction may spend, and what it is allowed to make smaller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Verification replays; one number does the same work on any machine.
    pub replays: u32,
    /// Which passes run.
    pub keep: Keep,
    /// The keys of the scenario that name a moment, which the moment pass brings forward; none
    /// where the caller names none. The scenario's schema is the world's, so the world names them.
    pub moments: &'static [&'static str],
}

impl Default for Limits {
    fn default() -> Self {
        Self { replays: 2000, keep: Keep::Both, moments: &[] }
    }
}

/// What `--keep` selects.
///
/// It names what is minimised; `Events` also writes tape bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keep {
    /// What the world did.
    Events,
    /// What the tape decided.
    Tape,
    /// Both.
    Both,
}

/// Which pass is running.
///
/// Passes run in this order, which removes most per replay. The configuration goes first because
/// it is read before the tape, and removing an entry shifts every draw after it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pass {
    /// Removing what the world was told to do, and bringing forward what is left.
    Config,
    /// Removing the faults that fired, then firing the remaining node faults sooner.
    Faults,
    /// Not delivering what the world chose to deliver.
    Events,
    /// Replacing what one decision drew with the plainest bytes it could have drawn.
    Tape,
    /// Nothing left to try.
    Done,
}

/// Replays spent, per pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Spent {
    /// Establishing what is being reduced, and confirming the result.
    pub verifying: u32,
    /// On the faults the run drew.
    pub faults: u32,
    /// On the configuration.
    pub config: u32,
    /// On deliveries.
    pub events: u32,
    /// On the tape.
    pub tape: u32,
}

impl Spent {
    /// Replays spent altogether.
    #[must_use]
    pub const fn total(self) -> u32 {
        self.verifying + self.faults + self.config + self.events + self.tape
    }
}

/// What a reduction knows about the scenario it started from.
#[derive(Clone, Debug)]
pub struct Baseline {
    /// What went wrong, in the form a candidate is compared against.
    pub violation: Violation,
    /// What the run cost.
    pub size: Size,
    /// What it drew, and where.
    pub journal: Journal,
}

/// Position of a candidate, so an answer cannot be attributed to a question nobody asked.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct CandidateId(pub u32);

/// One scenario a reduction wants carried out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Candidate {
    /// Which question this is.
    pub id: CandidateId,
    /// What to run.
    pub scenario: Scenario,
}

/// What carrying one out amounted to.
///
/// A subset of the run report, so the reducer does not depend on the kernel's output shape.
#[derive(Clone, Debug)]
pub struct CandidateOutcome {
    /// The candidate this answers.
    pub id: CandidateId,
    /// What went wrong, where anything did.
    pub violation: Option<Violation>,
    /// What the run cost.
    pub size: Size,
    /// What it drew, and where — what the next candidate resolves its edits against.
    pub journal: Journal,
}

/// Replays a reduction sets aside before it proposes anything.
///
/// One to establish the baseline and one to confirm the result; never spent on probes.
const RESERVED: u32 = 2;

/// How the remaining budget is divided.
///
/// The fault pass gets a fifth. A run against a real client fires only a handful of faults, and
/// one kill fired earlier removes more of the run than any other edit. The configuration pass
/// has few entries to try, so it needs little. The event pass gets the most, since most of a
/// cluster's reduction happens there. The tape pass rarely changes the answer and gets the rest.
const SHARE_FAULTS: u32 = 20;
const SHARE_CONFIG: u32 = 15;
const SHARE_EVENTS: u32 = 40;

/// A reduction in progress.
///
/// Each probe depends on the last answer. This type decides and never executes, so it is tested
/// without a process.
#[derive(Clone, Debug)]
pub struct Reduction {
    best: Scenario,
    best_size: Size,
    baseline: Violation,
    journal: Journal,
    limits: Limits,
    spent: Spent,
    pass: Pass,
    next_id: u32,
    /// The candidate proposed and not yet answered, with what accepting it would mean.
    outstanding: Option<(Candidate, Move)>,
    config: PassState,
    faults: FaultState,
    /// Scenarios already answered, by fingerprint, so none is replayed twice.
    ///
    /// Minimisation re-proposes refused subsets after a success, and two passes can reach the same
    /// scenario. On a client whose replay takes a minute, those repeats would spend the budget. A
    /// hash collision skips a candidate; it never gives a wrong answer.
    answered: BTreeSet<bohr_types::Digest>,
}

/// Where the fault pass has got to.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct FaultState {
    /// Whether removing faults has converged and moving them has begun.
    removed: bool,
    /// Which fired node fault is being moved, counted from the first. Faults move last to first.
    nth: Option<usize>,
    /// Positions among its earlier chances still to try, soonest last. `None` until built.
    ladder: Option<Vec<usize>>,
    /// Positions this fault has already been refused at.
    refused: Vec<usize>,
}

/// What accepting the outstanding candidate would do to the reduction.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Move {
    /// Keep these configuration entries.
    KeepEntries(Vec<usize>),
    /// Bring the moment at this position forward to this tick.
    Sooner { at: usize, to: u64 },
    /// Leave these decisions alone and make the rest go the dull way.
    KeepDecisions,
    /// Fire the node fault being moved at this earlier chance, which sits at this offset.
    FaultSooner { to: usize, at: u64 },
}

/// Where the configuration pass has got to.
#[derive(Clone, PartialEq, Eq, Debug)]
struct PassState {
    /// Minimisation over the entries, once there is more than one.
    entries: Option<DdMin>,
    /// The moment being brought forward, and the ladder of ticks still to try for it.
    moment: usize,
    ladder: Vec<u64>,
    /// Ticks this moment has already been refused at.
    ///
    /// A success rebuilds the ladder; refused ticks are skipped on the new one.
    refused: Vec<u64>,
    /// Minimisation over the decisions the pass on the tape is working through.
    decisions: Option<DdMin>,
}

impl Reduction {
    /// A reduction of `input`, which is already known to reproduce `baseline`.
    ///
    /// # Errors
    ///
    /// Refuses a budget smaller than the reserved replays.
    pub fn new(input: Scenario, baseline: Baseline, limits: Limits) -> Result<Self, ReducerError> {
        if limits.replays < RESERVED + 1 {
            return Err(ReducerError::BudgetTooSmall {
                given: limits.replays,
                least: RESERVED + 1,
            });
        }
        let mut reduction = Self {
            best: input,
            best_size: baseline.size,
            baseline: baseline.violation,
            journal: baseline.journal,
            limits,
            spent: Spent { verifying: RESERVED, ..Spent::default() },
            pass: match limits.keep {
                Keep::Tape => Pass::Tape,
                Keep::Events | Keep::Both => Pass::Config,
            },
            next_id: 0,
            outstanding: None,
            config: PassState {
                entries: None,
                moment: 0,
                ladder: Vec::new(),
                refused: Vec::new(),
                decisions: None,
            },
            faults: FaultState::default(),
            answered: BTreeSet::new(),
        };
        reduction.answered.insert(fingerprint(&reduction.best));
        reduction.propose();
        Ok(reduction)
    }

    /// The next scenario to carry out, or nothing where the budget is spent or every pass has
    /// converged.
    ///
    /// A pure function of the state, so a lost run can be reissued.
    #[must_use]
    pub fn next(&self) -> Option<&Candidate> {
        self.outstanding.as_ref().map(|(candidate, _)| candidate)
    }

    /// The smallest scenario proven to reproduce, which is the input until something beats it.
    #[must_use]
    pub const fn best(&self) -> &Scenario {
        &self.best
    }

    /// What the best scenario costs.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.best_size
    }

    /// Replays spent, per pass.
    #[must_use]
    pub const fn spent(&self) -> Spent {
        self.spent
    }

    /// Which pass is running.
    #[must_use]
    pub const fn pass(&self) -> Pass {
        self.pass
    }

    /// Folds in what happened to the candidate [`Self::next`] proposed.
    ///
    /// # Panics
    ///
    /// Panics if the outcome answers a candidate that was not proposed.
    pub fn accept(&mut self, outcome: &CandidateOutcome) {
        let (candidate, applied) =
            self.outstanding.take().expect("an outcome arrived for no candidate");
        assert_eq!(candidate.id, outcome.id, "an outcome answered a candidate nobody proposed");

        match self.pass {
            Pass::Faults => self.spent.faults += 1,
            Pass::Config => self.spent.config += 1,
            Pass::Events => self.spent.events += 1,
            Pass::Tape => self.spent.tape += 1,
            Pass::Done => {}
        }

        // Accepted only if it reproduces and is no larger.
        let kept =
            outcome.violation.as_ref().is_some_and(|found| self.baseline.reproduced_by(found))
                && outcome.size <= self.best_size;
        self.answered.insert(fingerprint(&candidate.scenario));
        if kept {
            self.best = candidate.scenario;
            self.best_size = outcome.size;
            self.journal = outcome.journal.clone();
        }
        self.fold_move(&applied, kept);
        self.propose();
    }

    /// Tells the pass what happened, so it knows what to ask next.
    fn fold_move(&mut self, applied: &Move, kept: bool) {
        match applied {
            Move::KeepEntries(_) => {
                // A removal renumbers entries, so minimisation restarts on the smaller document.
                if kept {
                    self.config.entries = None;
                } else if let Some(ddmin) = self.config.entries.as_mut() {
                    ddmin.accept(false);
                }
            }
            Move::KeepDecisions => {
                if let Some(ddmin) = self.config.decisions.as_mut() {
                    ddmin.accept(kept);
                }
                // Offsets shifted, so the working set is re-read from the new journal.
                if kept {
                    self.config.decisions = None;
                }
            }
            Move::FaultSooner { to, at } => {
                // Earlier refusals still hold. A kept candidate that did not fire the fault at the
                // new chance counts that rung as refused.
                if kept && fired_at(&self.journal, *at) {
                    let refused = self.faults.refused.clone();
                    let mut ladder = rungs_below(*to);
                    ladder.retain(|rung| !refused.contains(rung));
                    self.faults.ladder = Some(ladder);
                } else if let Some(ladder) = self.faults.ladder.as_mut()
                    && let Some(rung) = ladder.pop()
                {
                    self.faults.refused.push(rung);
                }
            }
            Move::Sooner { .. } => {
                if kept {
                    // The ladder descends from the success, skipping refused rungs.
                    self.config.ladder = ladder_for(self.current_moment_ticks());
                    let refused = self.config.refused.clone();
                    self.config.ladder.retain(|rung| !refused.contains(rung));
                } else if let Some(rung) = self.config.ladder.pop() {
                    self.config.refused.push(rung);
                }
                if self.config.ladder.is_empty() {
                    self.config.moment += 1;
                    // A new moment starts with no refusals.
                    self.config.refused.clear();
                    self.config.ladder = ladder_for(self.current_moment_ticks());
                }
            }
        }
    }

    /// The tick the moment now being worked on is written at, or zero where there is none.
    fn current_moment_ticks(&self) -> u64 {
        ConfigDoc::parse(self.best.config())
            .moments(self.limits.moments)
            .get(self.config.moment)
            .map_or(0, |moment| moment.ticks)
    }

    /// Works out what to ask next, moving between passes as each converges.
    fn propose(&mut self) {
        loop {
            if self.spent.total() >= self.limits.replays {
                self.pass = Pass::Done;
                self.outstanding = None;
                return;
            }
            let proposal = match self.pass {
                Pass::Faults if self.spent.faults < self.share(SHARE_FAULTS) => {
                    self.propose_faults()
                }
                Pass::Config if self.spent.config < self.share(SHARE_CONFIG) => {
                    self.propose_config()
                }
                Pass::Events if self.spent.events < self.share(SHARE_EVENTS) => {
                    self.propose_bytes(Target::Deliveries)
                }
                Pass::Tape => self.propose_bytes(Target::Everything),
                Pass::Done => {
                    self.outstanding = None;
                    return;
                }
                Pass::Faults | Pass::Config | Pass::Events => None,
            };
            let Some((candidate, applied)) = proposal else {
                self.enter_next_pass();
                continue;
            };
            // A scenario already answered counts as a refusal and is not replayed.
            if self.answered.contains(&fingerprint(&candidate.scenario)) {
                self.fold_move(&applied, false);
                continue;
            }
            self.outstanding = Some((candidate, applied));
            return;
        }
    }

    /// How many replays a share of the budget is worth.
    fn share(&self, percent: u32) -> u32 {
        let usable = self.limits.replays.saturating_sub(RESERVED);
        usable * percent / 100
    }

    /// The pass after this one, given what the caller asked to minimise.
    fn enter_next_pass(&mut self) {
        self.pass = match (self.pass, self.limits.keep) {
            (Pass::Config, _) => Pass::Faults,
            (Pass::Faults, _) => Pass::Events,
            (Pass::Events, Keep::Events) => Pass::Done,
            (Pass::Events, _) => Pass::Tape,
            _ => Pass::Done,
        };
        // Each pass reads its working set from the current best journal.
        self.config.decisions = None;
    }

    /// The next configuration candidate: an entry to remove, or a moment to bring forward.
    fn propose_config(&mut self) -> Option<(Candidate, Move)> {
        let doc = ConfigDoc::parse(self.best.config());

        // Entries first, since a removal takes its consequences with it. A single entry is tried
        // too.
        if self.config.entries.is_none() && !doc.entries().is_empty() {
            self.config.entries = Some(DdMin::new(doc.entries().len()));
        }
        if let Some(ddmin) = self.config.entries.as_ref()
            && let Some(keep) = ddmin.next()
        {
            let keep = keep.to_vec();
            let scenario = self.best.with_config(doc.keeping(&keep).to_toml());
            return Some((self.candidate(scenario), Move::KeepEntries(keep)));
        }

        // Then moments, brought forward to shorten the run.
        loop {
            let moments = doc.moments(self.limits.moments);
            let moment = moments.get(self.config.moment)?;
            if self.config.ladder.is_empty() {
                self.config.ladder = ladder_for(moment.ticks);
                if self.config.ladder.is_empty() {
                    self.config.moment += 1;
                    continue;
                }
            }
            let to = *self.config.ladder.last()?;
            let scenario = self.best.with_config(doc.moved(moment, to).to_toml());
            let at = self.config.moment;
            return Some((self.candidate(scenario), Move::Sooner { at, to }));
        }
    }

    /// The next fault candidate: faults to stop firing, then a node fault to fire sooner.
    fn propose_faults(&mut self) -> Option<(Candidate, Move)> {
        if !self.faults.removed {
            if let Some(proposal) = self.propose_bytes(Target::Faults) {
                return Some(proposal);
            }
            self.faults.removed = true;
            self.config.decisions = None;
        }

        // Faults move last to first, so a move never disturbs a later fault's bytes.
        loop {
            let nth = match self.faults.nth {
                Some(nth) => nth,
                None => {
                    let count = fired_node_faults(&self.journal);
                    let last = count.checked_sub(1)?;
                    self.faults.nth = Some(last);
                    last
                }
            };
            let Some(fault) = movable(&self.journal, self.best.prefix(), nth) else {
                self.faults.nth = Some(nth.checked_sub(1)?);
                self.faults.ladder = None;
                self.faults.refused.clear();
                continue;
            };
            let refused = self.faults.refused.clone();
            let ladder = self.faults.ladder.get_or_insert_with(|| {
                let mut ladder = rungs_below(fault.chances.len());
                ladder.retain(|rung| !refused.contains(rung));
                ladder
            });
            let Some(&to) = ladder.last() else {
                self.faults.nth = Some(nth.checked_sub(1)?);
                self.faults.ladder = None;
                self.faults.refused.clear();
                continue;
            };
            let at = *fault.chances.get(to)?;
            let scenario = self.best.with_bytes(&sooner(at, &fault.body));
            return Some((self.candidate(scenario), Move::FaultSooner { to, at }));
        }
    }

    /// The next candidate for a pass that rewrites decisions on the tape.
    ///
    /// The working set comes from the current best run's journal, re-read after each success.
    fn propose_bytes(&mut self, target: Target) -> Option<(Candidate, Move)> {
        let reachable: Vec<_> = decisions(&self.journal, self.best.prefix().len(), target)
            .into_iter()
            .filter(|decision| !decision.is_already_dull(self.best.prefix(), target))
            .collect();
        if reachable.is_empty() {
            return None;
        }
        if self.config.decisions.is_none() {
            self.config.decisions = Some(DdMin::new(reachable.len()));
        }
        let keep = self.config.decisions.as_ref()?.next()?.to_vec();

        let edits: Vec<_> = reachable
            .iter()
            .enumerate()
            .filter(|(at, _)| !keep.contains(at))
            .map(|(_, decision)| decision.edit(target))
            .collect();
        if edits.is_empty() {
            return None;
        }
        let scenario = self.best.with_bytes(&edits);
        Some((self.candidate(scenario), Move::KeepDecisions))
    }

    /// Wraps a scenario as the next question.
    fn candidate(&mut self, scenario: Scenario) -> Candidate {
        let id = CandidateId(self.next_id);
        self.next_id += 1;
        Candidate { id, scenario }
    }
}

/// Positions to try among `count` earlier chances, soonest last.
///
/// A geometric ladder; four probes show whether the fault can fire near the start.
fn rungs_below(count: usize) -> Vec<usize> {
    let mut rungs: Vec<usize> =
        [count / 2, count / 4, count / 8, 0].into_iter().filter(|rung| *rung < count).collect();
    rungs.dedup();
    rungs
}

/// A scenario's identity, for knowing it has been answered.
///
/// A digest of its seed, its prefix and its configuration, each length-prefixed, so it is the same
/// under every release of the compiler.
fn fingerprint(scenario: &Scenario) -> bohr_types::Digest {
    let mut bytes = scenario.seed().get().to_le_bytes().to_vec();
    for part in [scenario.prefix(), scenario.config().as_bytes()] {
        bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
        bytes.extend_from_slice(part);
    }
    bohr_types::Digest::of(&bytes)
}

/// The ticks to try for a moment, soonest last.
///
/// A geometric ladder: four probes show whether the moment can move to the start. Bisecting to
/// the exact tick would cost dozens of replays.
fn ladder_for(ticks: u64) -> Vec<u64> {
    if ticks < 2 {
        return Vec::new();
    }
    let mut rungs: Vec<u64> = [ticks / 2, ticks / 4, ticks / 8, 1]
        .into_iter()
        .filter(|rung| *rung < ticks && *rung >= 1)
        .collect();
    rungs.dedup();
    rungs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scenario's fingerprint is a digest of its parts, the same under every compiler.
    #[test]
    fn a_fingerprint_is_the_same_under_every_compiler() {
        let scenario = Scenario::new(bohr_types::Seed(7), vec![1, 2, 3], "[faults]\n".to_owned());
        assert_eq!(fingerprint(&scenario), fingerprint(&scenario.clone()));
        let other =
            Scenario::new(bohr_types::Seed(7), vec![1, 2, 3], "[faults]\nkill = 1\n".to_owned());
        assert_ne!(fingerprint(&scenario), fingerprint(&other));
        // Pinned: a change of algorithm is a change of what a reduction skips.
        assert_eq!(
            fingerprint(&scenario).to_hex(),
            bohr_types::Digest::of(&{
                let mut bytes = 7u64.to_le_bytes().to_vec();
                bytes.extend_from_slice(&3u64.to_le_bytes());
                bytes.extend_from_slice(&[1, 2, 3]);
                bytes.extend_from_slice(&9u64.to_le_bytes());
                bytes.extend_from_slice(b"[faults]\n");
                bytes
            })
            .to_hex()
        );
    }

    #[test]
    fn a_ladder_descends_to_the_first_tick_and_never_climbs() {
        let rungs = ladder_for(9_000_000_000);
        assert_eq!(rungs.last(), Some(&1), "a ladder that never reaches the start: {rungs:?}");
        for pair in rungs.windows(2) {
            assert!(pair[0] > pair[1], "a ladder that climbs: {rungs:?}");
        }
        assert!(ladder_for(1).is_empty(), "the first tick has nowhere to move to");
        assert!(ladder_for(0).is_empty());
    }

    #[test]
    fn a_budget_that_cannot_prove_its_answer_is_refused() {
        let scenario = Scenario::new(bohr_types::Seed(1), vec![0; 4], String::new());
        let baseline = Baseline {
            violation: Violation::of(
                &bohr_trace::Outcome::GuestHung("stuck".to_owned()),
                bohr_kernel::StopReason::TickBudget,
                &[],
            )
            .expect("a finding"),
            size: Size { events: 10, draws: 2 },
            journal: Journal::new(),
        };
        let refused = Reduction::new(
            scenario,
            baseline,
            Limits { replays: 2, keep: Keep::Both, moments: &[] },
        );
        assert!(matches!(refused, Err(ReducerError::BudgetTooSmall { .. })));
    }
}
