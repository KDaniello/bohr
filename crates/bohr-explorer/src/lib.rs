//! The search: a corpus of tapes, the mutations that grow it, and the campaign that folds results.
//!
//! Sans-io like the kernel (`docs/ARCHITECTURE.md` §5.11): nothing here spawns a process, opens a
//! file or reads a clock. A campaign takes run results and answers with work, so the whole search
//! is tested in milliseconds on any machine.
//!
//! Work is planned a generation at a time and results are folded in planned order, so worker count
//! and speed never reach a decision.

#![forbid(unsafe_code)]

mod campaign;
mod chaos;
mod choices;
mod corpus;
mod mutate;
mod properties;
mod state;

pub use campaign::{Campaign, Finding, Progress, RunResult, WorkItem};
pub use choices::Choices;
pub use corpus::{Corpus, DEFAULT_CAPACITY, Entry, Offer, Origin};
pub use mutate::{Context, MAX_PREFIX_BYTES, Operator, mutate};
pub use properties::{MOST_PROPERTIES, Properties, Tally};
pub use state::StateError;
