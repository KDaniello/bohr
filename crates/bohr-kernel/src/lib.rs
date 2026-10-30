//! Logical time, scheduling and the run loop, plus the ports the run loop drives.
//!
//! The kernel declares [`WorldModel`], [`Oracle`] and [`SchedulePolicy`] and implements none of
//! them. It is depended upon by its plug-ins rather than depending on them, so a second domain adds
//! a sibling crate instead of editing this one. See `docs/ARCHITECTURE.md` §5.16 and §5.17.
//!
//! Nothing here performs I/O or reads a host clock. The loop is a state machine over
//! `(stops, tape)`, which is what lets the layers above be tested in milliseconds on any operating
//! system.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(any(feature = "chaos-hash-iteration", feature = "chaos-delivery-tie"))]
mod chaos;
mod clock;
mod ports;
mod run;
mod schedule;

pub use clock::{Clock, NodeClock, MAX_DRIFT_PPM};
pub use ports::{
    Diagnostic, DueEffects, Effects, EventSink, FaultCatalog, NodeOp, Oracle, RunOutcome,
    TaskState, Verdict, Wake, WorldCtx, WorldModel,
};
pub use run::{run, RunConfig, RunInputs, RunReport};
pub use schedule::{PctPolicy, RandomPolicy, SchedulePolicy};

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
    /// The code belongs to the refusal rather than to this variant: an unmodelled syscall and a
    /// stop kind the model does not implement are different failures with different remedies.
    #[error("{code}: {message}")]
    World {
        /// Catalogue code, as listed in `docs/API.md` §8.
        code: &'static str,
        /// What was refused.
        message: String,
    },

    /// `BOHR-5001` — the loop reached a state it does not define behaviour for.
    #[error("BOHR-5001: kernel invariant violated: {0}")]
    Invariant(&'static str),
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
    /// Not a budget. A budget is what stops a guest that is not going to end on its own, and the
    /// answer to one is "this is hung". A horizon is what a scenario asks for when the guest is a
    /// server: reaching it is the whole of the execution that was wanted, and a run that reached
    /// it is finished rather than abandoned.
    HorizonReached,
    /// The node whose ending the scenario said the run's ending is has ended.
    ///
    /// The same kind of answer a horizon is, arrived at by watching rather than by waiting. A
    /// cluster of a service and the thing that drives it has no ending of its own — the service
    /// goes on serving — so what the run was for is over when the driver is done, and every tick
    /// after that is a server talking to nobody.
    NodeFinished,
}
