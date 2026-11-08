//! A reduction in progress: what has been proven smaller, and what to try next.

use bohr_tape::Journal;

use crate::{
    ReducerError,
    bytes::{Target, decisions},
    config::ConfigDoc,
    ddmin::DdMin,
    scenario::{Scenario, Size},
    violation::Violation,
};

/// What a reduction may spend, and what it is allowed to make smaller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Verification replays. The reproducible budget: two reductions given the same number do the
    /// same work, whatever the machine.
    pub replays: u32,
    /// Which passes run.
    pub keep: Keep,
}

impl Default for Limits {
    fn default() -> Self {
        Self { replays: 2000, keep: Keep::Both }
    }
}

/// What `--keep` selects.
///
/// It names what is *minimised*, never what is touched: not delivering a message is a tape edit in
/// this design, so `Events` writes tape bytes too.
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pass {
    /// Removing what the world was told to do, and bringing forward what is left.
    Config,
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
        self.verifying + self.config + self.events + self.tape
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
/// Four facts decide a reduction, and taking the whole run report would tie it to the shape of the
/// kernel's output — the same division the search makes, for the same reason.
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
/// One to establish what is being reduced, one to confirm the result. Never spent on a probe: a
/// reduction that had used its whole budget guessing would have nothing left to prove its answer.
const RESERVED: u32 = 2;

/// How the remaining budget is divided.
///
/// The event pass is where the collapse happens, so it gets half. The configuration pass is cheap
/// per candidate and enormous per success, and there are few entries to try, so it needs little.
/// The tape pass is the long tail: it improves the number and rarely changes the answer, so it
/// runs last on what is left.
const SHARE_CONFIG: u32 = 15;
const SHARE_EVENTS: u32 = 50;

/// A reduction in progress.
///
/// Adaptive where a campaign is generational: the next probe is a function of the last answer, so
/// there is nothing to plan a batch of. What survives from the campaign's shape is the division
/// that matters — this type decides and never executes, so every claim it makes is provable
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
    /// A success rebuilds the ladder from the new tick, and every ladder ends at one -- so a
    /// moment that had already failed at tick one was offered it again after each success, and a
    /// replay went on learning an answer the reduction already had.
    refused: Vec<u64>,
    /// Minimisation over the decisions the pass on the tape is working through.
    decisions: Option<DdMin>,
}

impl Reduction {
    /// A reduction of `input`, which is already known to reproduce `baseline`.
    ///
    /// # Errors
    ///
    /// Refuses a budget that cannot cover the replays a reduction spends before proposing
    /// anything, because the alternative is a reduction that stops before it starts and reports
    /// its input as its answer.
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
        };
        reduction.propose();
        Ok(reduction)
    }

    /// The next scenario to carry out, or nothing where the budget is spent or every pass has
    /// converged.
    ///
    /// A pure function of the state: asking twice without accepting anything proposes the same
    /// candidate, which is what a run that was lost is reissued from.
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
    /// Panics if the outcome answers a candidate that was not proposed. A reduction that accepted
    /// an answer to a question it did not ask would keep a scenario nobody verified.
    pub fn accept(&mut self, outcome: &CandidateOutcome) {
        let (candidate, applied) =
            self.outstanding.take().expect("an outcome arrived for no candidate");
        assert_eq!(candidate.id, outcome.id, "an outcome answered a candidate nobody proposed");

        match self.pass {
            Pass::Config => self.spent.config += 1,
            Pass::Events => self.spent.events += 1,
            Pass::Tape => self.spent.tape += 1,
            Pass::Done => {}
        }

        // Reproduced *and* no larger. A candidate that still goes wrong but records more is not a
        // reduction, and keeping it would make the reported numbers wander upward.
        let kept =
            outcome.violation.as_ref().is_some_and(|found| self.baseline.reproduced_by(found))
                && outcome.size <= self.best_size;
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
                // A removal renumbers what is left, so the positions this minimisation is holding
                // stop meaning what they meant and it starts again over the smaller document.
                // That re-tries some subsets; addressing entries that no longer exist would
                // remove the wrong ones, which is not a trade.
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
                // A success changed what the run does next, so every offset after it belongs to
                // something else and the working set has to be read from the new journal.
                if kept {
                    self.config.decisions = None;
                }
            }
            Move::Sooner { .. } => {
                if kept {
                    // A moment that moved is a moment worth trying to move further, so the ladder
                    // keeps descending from where it succeeded -- minus the rungs it has already
                    // been refused at, which are answers this reduction has and would pay for
                    // again.
                    self.config.ladder = ladder_for(self.current_moment_ticks());
                    let refused = self.config.refused.clone();
                    self.config.ladder.retain(|rung| !refused.contains(rung));
                } else if let Some(rung) = self.config.ladder.pop() {
                    self.config.refused.push(rung);
                }
                if self.config.ladder.is_empty() {
                    self.config.moment += 1;
                    // A different moment has its own history; what this one was refused at says
                    // nothing about the next.
                    self.config.refused.clear();
                    self.config.ladder = ladder_for(self.current_moment_ticks());
                }
            }
        }
    }

    /// The tick the moment now being worked on is written at, or zero where there is none.
    fn current_moment_ticks(&self) -> u64 {
        ConfigDoc::parse(self.best.config())
            .moments()
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
            match self.pass {
                Pass::Config => {
                    if self.spent.config >= self.share(SHARE_CONFIG) {
                        self.enter_next_pass();
                        continue;
                    }
                    if let Some(proposal) = self.propose_config() {
                        self.outstanding = Some(proposal);
                        return;
                    }
                    self.enter_next_pass();
                }
                Pass::Events => {
                    if self.spent.events >= self.share(SHARE_EVENTS) {
                        self.enter_next_pass();
                        continue;
                    }
                    if let Some(proposal) = self.propose_bytes(Target::Deliveries) {
                        self.outstanding = Some(proposal);
                        return;
                    }
                    self.enter_next_pass();
                }
                Pass::Tape => {
                    if let Some(proposal) = self.propose_bytes(Target::Everything) {
                        self.outstanding = Some(proposal);
                        return;
                    }
                    self.enter_next_pass();
                }
                Pass::Done => {
                    self.outstanding = None;
                    return;
                }
            }
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
            (Pass::Config, _) => Pass::Events,
            (Pass::Events, Keep::Events) => Pass::Done,
            (Pass::Events, _) => Pass::Tape,
            _ => Pass::Done,
        };
        // Each pass reads its own working set out of the journal of whatever is best by the time
        // it starts, so nothing is carried across from the pass before it.
        self.config.decisions = None;
    }

    /// The next configuration candidate: an entry to remove, or a moment to bring forward.
    fn propose_config(&mut self) -> Option<(Candidate, Move)> {
        let doc = ConfigDoc::parse(self.best.config());

        // Entries first. Removing one takes everything it caused with it, which is worth more per
        // replay than any amount of moving what stays.
        // **One entry is one entry to try removing.** Minimisation started only at two, and
        // `DdMin::new(1)` is finished before it begins, so a scenario carrying a single `[[fault]]`
        // that the finding does not need kept it for ever -- a result that is not one-minimal,
        // against the promise the ddmin module opens with. One replay buys the answer.
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

        // Then the moments. A scenario whose decisive fault fires at the first tick is the same
        // scenario reached in a fraction of the events.
        loop {
            let moments = doc.moments();
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

    /// The next candidate for a pass that rewrites decisions on the tape.
    ///
    /// The working set is read from the journal of the run that is currently best, so a success
    /// starts the pass again over what the smaller run actually decided rather than over what its
    /// predecessor did.
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

/// The ticks to try for a moment, soonest last.
///
/// A geometric ladder rather than a search: what matters is whether the decisive moment can happen
/// at the start of the run instead of near its end, and four probes answer that. Bisecting to the
/// exact earliest tick would spend dozens of replays to make a number in a configuration file
/// prettier.
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
        let refused = Reduction::new(scenario, baseline, Limits { replays: 2, keep: Keep::Both });
        assert!(matches!(refused, Err(ReducerError::BudgetTooSmall { .. })));
    }
}
