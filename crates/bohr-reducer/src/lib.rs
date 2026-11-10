//! Delta debugging over a scenario: the smallest world that still breaks the guest.
//!
//! Sans-io: a reduction proposes a candidate and is told the result, so its guarantees (the result
//! reproduces and is never larger) are tested in milliseconds without a guest.
//!
//! # What is reduced
//!
//! A scenario is a seed, a tape prefix and a world configuration. Reduction removes what the world
//! did (partitions, faults, deliveries); the guest program is unchanged. A run gets shorter because
//! the world interferes earlier.
//!
//! # One candidate at a time
//!
//! Each probe depends on the last answer, so batching would waste the budget; 2000 replays of a
//! traced cluster take minutes.

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
