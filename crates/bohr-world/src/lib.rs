//! A world model that answers clock, randomness and guest output for a single node.
//!
//! It replaces the things a guest most commonly reaches the outside world for. Anything else is
//! refused by name rather than passed through or stubbed: a syscall this model does not understand
//! is a hole in determinism, and a hole that reports itself is cheaper than one that does not. See
//! `docs/ARCHITECTURE.md` §5.7.

#![forbid(unsafe_code)]

#[cfg(any(
    feature = "chaos-syscall-passthru",
    feature = "chaos-futex-order",
    feature = "chaos-node-clock-shared",
    feature = "chaos-deadline-tie",
    feature = "chaos-fault-moment-drift",
    feature = "chaos-unknown-descriptor",
    feature = "chaos-proc-on-the-disk"
))]
mod chaos;
pub mod config;
pub mod cpu;
mod distributed;
mod minimal;
mod nums;
mod oracle;
mod projection;
mod ring;
mod state;

pub use config::{
    Cluster, ConfigError, Differential, Implementation, IoUring, LinuxConfig, LoadedConfig,
};
pub use distributed::{
    DiskConfig, DistributedWorld, DrawnFaults, DurabilityConfig, FaultPolicy, GreyWindow,
    LivenessConfig, NetworkConfig, NodeFaultKind, ScheduledNodeFault, ScheduledPartition,
    Targeting, Topology, sdk_violation_code,
};
pub use minimal::{MinimalWorld, OutputSink};
pub use oracle::{
    AgreementOracle, DurabilityOracle, LivenessOracle, Panel, PropertiesAndCrashOracle, Watching,
    panel,
};
pub use projection::{MAX_PROJECTION_BYTES, Projected, ProjectionSpec, Source, Value};
pub use state::{StateError, StateImage};
