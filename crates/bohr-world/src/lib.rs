//! A world model that answers clock, randomness and guest output for a single node.
//!
//! Anything else is refused by name, never passed through or stubbed. See
//! `docs/ARCHITECTURE.md` §5.7.

#![forbid(unsafe_code)]

mod bytes;
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
pub mod probe;
mod projection;
mod ring;
mod state;

pub use config::{
    Cluster, ConfigError, Differential, Implementation, IoUring, LinuxConfig, LoadedConfig,
};
pub use distributed::{
    DiskConfig, DistributedWorld, DrawnFaults, DurabilityConfig, FaultPolicy, GreyWindow,
    LivenessConfig, NetworkConfig, NodeFaultKind, ScheduledNodeFault, ScheduledPartition,
    Suppressed, Targeting, Topology, sdk_violation_code,
};
pub use minimal::{MinimalWorld, OutputSink};
pub use oracle::{
    AgreementOracle, DurabilityOracle, LivenessOracle, Panel, ProbeOracle, Probed,
    PropertiesAndCrashOracle, Watching, panel,
};
pub use probe::{ProbeSpec, Property};
pub use projection::{MAX_PROJECTION_BYTES, Projected, ProjectionSpec, Source, Value};
pub use state::{StateError, StateImage};
