//! The search: a corpus of tapes, the mutations that grow it, and the campaign that folds results.
//!
//! Sans-io like the kernel, and for the same reason (`docs/ARCHITECTURE.md` §5.11): nothing here
//! spawns a process, opens a file or reads a clock. A campaign is a state machine that is handed
//! run results and answers with more work, which is what lets the whole search — corpus, mutation,
//! generations, the promise that eight workers find what one finds — be tested in milliseconds on
//! any machine, including one with no interceptor at all.
//!
//! The other half of that promise is the shape of a campaign, and it is the load-bearing decision
//! in this crate: work is planned a **generation** at a time from the corpus as it stood at the
//! end of the last one, and results are folded **in the order they were planned** rather than the
//! order they arrived. Worker count and worker speed therefore cannot reach any decision, because
//! they cannot reach the work list either.

#![forbid(unsafe_code)]

mod campaign;
mod chaos;
mod choices;
mod corpus;
mod mutate;
mod properties;

pub use campaign::{Campaign, Finding, Progress, RunResult, WorkItem};
pub use choices::Choices;
pub use corpus::{Corpus, DEFAULT_CAPACITY, Entry, Offer, Origin};
pub use mutate::{Context, MAX_PREFIX_BYTES, Operator, mutate};
pub use properties::{MOST_PROPERTIES, Properties, Tally};
