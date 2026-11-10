//! Logical time, scheduling and the run loop, plus the ports the run loop drives.
//!
//! The kernel declares [`WorldModel`], [`Oracle`] and [`SchedulePolicy`] and implements none.
//! Plug-ins depend on it, so a new domain is a sibling crate. See `docs/ARCHITECTURE.md` §5.16 and
//! §5.17.
//!
//! No I/O and no host clock: the loop is a state machine over `(stops, tape)`, testable in
//! milliseconds on any system.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(any(feature = "chaos-hash-iteration", feature = "chaos-delivery-tie"))]
mod chaos;
mod clock;
mod ports;
mod run;
mod schedule;

pub use clock::{Clock, MAX_DRIFT_PPM, NodeClock, WALL_EPOCH};
pub use ports::{
    Diagnostic, DueEffects, Effects, EventSink, FaultCatalog, NodeOp, Observer, Oracle, Progress,
    RunOutcome, TaskState, Verdict, Wake, WorldCtx, WorldModel,
};
pub use run::{
    ARMED_VARIABLE, ARMING, Arming, PROGRESS_EVERY, RunConfig, RunInputs, RunMetrics, RunReport,
    run,
};
pub use schedule::{FairPolicy, PctPolicy, RandomPolicy, SchedulePolicy};

/// Failures that originate in the kernel.
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    /// A backend operation failed.
    #[error(transparent)]
    Backend(#[from] bohr_backend::BackendError),

    /// A draw could not be made.
    #[error(transparent)]
    Tape(#[from] bohr_tape::TapeError),

    /// An event could not be recorded.
    #[error(transparent)]
    Trace(#[from] bohr_trace::TraceError),

    /// The world model refused to handle a stop, under the code naming why.
    ///
    /// The refusal carries its own code, since different refusals have different remedies.
    #[error("{code}: {message}")]
    World {
        /// Catalogue code, as listed in `docs/API.md` §8.
        code: &'static str,
        /// What was refused.
        message: String,
    },

    /// `BOHR-5001` — the loop reached a state it does not define behaviour for.
    ///
    /// Raised by the kernel and by worlds alike; [`crate::run`] hands it back as
    /// [`Self::InvariantAt`], with where in the run it happened.
    #[error("BOHR-5001: kernel invariant violated: {0}")]
    Invariant(&'static str),

    /// `BOHR-5001` — as [`Self::Invariant`], with the event, tick and task it happened at.
    #[error("BOHR-5001: kernel invariant violated: {what} (event {event}, tick {tick}, {task})")]
    InvariantAt {
        /// What was violated.
        what: &'static str,
        /// The next event the run would have recorded.
        event: u64,
        /// The world clock.
        tick: u64,
        /// The task being handled, or the one that last ran.
        task: String,
    },
}

/// Why a run stopped short of the guest finishing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopReason {
    /// Every task ended.
    GuestFinished,
    /// Every remaining task is waiting and nothing is left to release any of them.
    Deadlock,
    /// The virtual time budget was reached.
    TickBudget,
    /// The event budget was reached.
    EventBudget,
    /// A caller asked the world to stop at an event, and it did.
    BreakRequested,
    /// The run reached the virtual time it was asked to reach.
    ///
    /// A horizon is the run's intended length for a server guest; reaching it is a finished run.
    /// A budget, by contrast, means the guest hung.
    HorizonReached,
    /// The node whose ending the scenario said the run's ending is has ended.
    ///
    /// Like a horizon: a service never ends, so the run is over when its driver is.
    NodeFinished,
}
