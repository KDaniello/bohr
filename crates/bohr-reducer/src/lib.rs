//! Delta debugging over a scenario: the smallest world that still breaks the guest.
//!
//! Sans-io like the search it is a sibling of. Nothing here runs anything: a reduction proposes a
//! candidate and is told what happened to it, so every claim it makes — that the result still
//! reproduces, that it is never larger than the input — is provable in milliseconds against a
//! function, with no guest, no process and no operating system in the way.
//!
//! # What is reduced, and what cannot be
//!
//! A scenario is a seed, a tape prefix and a world configuration. The guest's program is not part
//! of it and cannot be made smaller: what reduction removes is what the *world* did — a partition,
//! a scheduled fault, a delivery — and the program that was done to is whatever it was compiled
//! as. A run therefore becomes short because the world stopped interfering earlier, never because
//! there was less guest left to execute.
//!
//! # Why it is adaptive where a campaign is generational
//!
//! A campaign plans a generation from its corpus and folds the answers back in any order, which is
//! what makes eight workers and one agree. Delta debugging cannot be planned that way: the next
//! probe is a function of the last answer, so a batch would spend its whole budget on questions
//! that a single earlier success made pointless. The budget is the scarce thing here — two
//! thousand replays of a traced cluster is minutes — so the shape is one candidate at a time.

#![forbid(unsafe_code)]

mod bytes;
mod config;
mod ddmin;
mod reduction;
mod scenario;
mod violation;

pub use config::{ConfigDoc, Entry, Moment};
pub use ddmin::DdMin;
pub use reduction::{
    Baseline, Candidate, CandidateId, CandidateOutcome, Keep, Limits, Pass, Reduction, Spent,
};
pub use scenario::{Edit, Scenario, Size};
pub use violation::{Class, FailedAssertion, Violation};

/// Why a reduction could not start.
#[derive(Debug, thiserror::Error)]
pub enum ReducerError {
    /// The scenario it was given does not reproduce anything.
    #[error("BOHR-5003: the scenario to reduce did not reproduce a finding")]
    NothingToReduce,
    /// The budget cannot cover the replays a reduction has to spend before it proposes anything.
    #[error("BOHR-1001: a reduction needs at least {least} replays, and was given {given}")]
    BudgetTooSmall {
        /// What it was given.
        given: u32,
        /// What it needs.
        least: u32,
    },
}
