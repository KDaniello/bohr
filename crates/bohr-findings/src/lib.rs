//! What a finding is, decided from what a run recorded: which group it falls in, what its ablation
//! found, what kind of finding it is, how a host without bohr stages it, and the facts a run
//! comes to.
//!
//! The rules, apart from the command line that reads directories, runs bundles and writes files,
//! so they are stated once and read the same wherever a finding is judged.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ablation;
pub mod confirm;
pub mod facts;
pub mod group;
pub mod triage;
