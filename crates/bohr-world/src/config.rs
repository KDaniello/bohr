//! The world configuration file: `bohr run --config cluster.toml`.
//!
//! The shape is normative in `docs/API.md` §5. Everything is validated at load, and every refusal
//! names the key it refuses: a configuration mistake found at tick four billion is a configuration
//! mistake this parser should have caught.
//!
//! Beside the models it configures rather than in the tool that reads the file: the text names
//! this crate's own types, and two callers now need it — the binary that runs a scenario, and the
//! harness that reduces one. A parser in the binary would have been copied rather than shared.

use bohr_types::{NodeId, Tick};
use serde::Deserialize;

use crate::{
    DiskConfig, DurabilityConfig, FaultPolicy, GreyWindow, LivenessConfig, NetworkConfig,
    NodeFaultKind, ProjectionSpec, ScheduledNodeFault, ScheduledPartition, Source, StateImage,
    Targeting, Topology,
};

/// The largest file a prepared state may hold.
///
/// The same bound the world puts on a file a guest writes, and for the same reason: every byte
/// here is held in supervisor memory, cloned into a snapshot ring at each `fsync`, once per node,
/// in every worker a campaign runs.
const MAX_STATE_FILE_BYTES: u64 = 1 << 26;

/// Why a configuration was refused.
///
/// One variant on purpose: every refusal is the same kind of thing — a key this build will not
/// accept the value of — and the message names the key. Splitting it would give callers a choice
/// they have no different reaction to.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The key named in the message cannot be accepted as written.
    #[error("{0}")]
    Refused(String),
}

/// Smallest coverage map that can tell one path from another at all.
const MIN_COVERAGE_BYTES: usize = 1 << 10;

/// Largest this build will hold, per worker, and copy once per run.
const MAX_COVERAGE_BYTES: usize = 1 << 26;

/// Largest denominator a probability may carry (`docs/API.md` §5.2).
const MAX_DENOMINATOR: u32 = 65_536;

/// Everything a configuration file can ask a run to be.
#[derive(Debug)]
pub struct LoadedConfig {
    /// The cluster shape, when one is configured. Consumed where a program can actually be
    /// traced; a build with no syscall backend still parses and validates it, then refuses.
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
    pub cluster: Option<Cluster>,
    /// What the distributed world is wired with.
    pub network: NetworkConfig,
    /// The disk's failure modes.
    pub disk: DiskConfig,
    /// Node faults on the scenario clock.
    pub node_faults: Vec<ScheduledNodeFault>,
    /// How drawn node faults choose their moment, where a campaign armed them.
    pub fault_policy: FaultPolicy,
    /// Bounds on the run itself, where the file names any.
    pub run: RunLimits,
    /// What the cluster promises about going on committing, where the file asks for it.
    pub liveness: Option<LivenessConfig>,
    /// Whether an answer given before a sync and lost to a crash is a finding.
    pub durability: Option<DurabilityConfig>,
    /// The filesystem every node starts from, where the scenario prepared one.
    pub state: Option<StateImage>,
    /// What `getcwd` answers, where the scenario says.
    pub cwd: Option<String>,
    /// The implementations a differential run compares, and the values it compares them by.
    pub differential: Option<Differential>,
    /// Counters in the coverage map, where the scenario sizes one.
    pub coverage_bytes: Option<usize>,
    /// The kernel the guest is shown, where the scenario names one other than the default.
    pub linux: LinuxConfig,
}

/// The `[linux]` section: which kernel facilities the guest is told exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LinuxConfig {
    /// Whether opening a submission ring succeeds.
    pub io_uring: IoUring,
}

/// What `io_uring_setup` is answered.
///
/// `Absent` is the default and not a fallback: a kernel built without the option, or one with
/// `kernel.io_uring_disabled=2`, answers `ENOSYS`, and every program that opens a ring keeps a path
/// for that reply. `Ring` opens one whose file operations are carried through the same model as the
/// plain calls, and whose other operations end the run by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum IoUring {
    /// `ENOSYS` at setup; entering or registering a ring ends the run under `BOHR-2004`.
    #[default]
    Absent,
    /// A ring, a polled one with its poller asleep, carrying the file operations in API.md §5.1.
    Ring,
}

/// One tape, several implementations, and the values they must agree on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Differential {
    /// What each implementation is called and how it is started.
    pub implementations: Vec<Implementation>,
    /// The values every implementation must produce equally.
    pub projections: Vec<ProjectionSpec>,
}

/// One implementation under comparison.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Implementation {
    /// What it is called, in the configuration and in the report.
    pub name: String,
    /// How it is started, with `{node}` substituted as the cluster's argv is.
    ///
    /// Empty where the scenario names none, and then the node runs whatever it would have run
    /// without a comparison — its cluster program, or the command line where there is no cluster.
    pub argv: Vec<String>,
    /// Which node's program this stands in for.
    ///
    /// A cluster of unlike nodes is how a real client is driven at all: one node is the thing
    /// under comparison and another is the caller that makes it do anything. Only one of them
    /// is the implementation, and this says which — without it the caller would be replaced
    /// too, and two implementations would be compared by a value neither of them produced.
    pub node: u32,
}

impl LoadedConfig {
    /// What this scenario asks to be judged on.
    #[must_use]
    pub const fn watching(&self) -> crate::Watching {
        crate::Watching { liveness: self.liveness.is_some(), durability: self.durability.is_some() }
    }

    /// Whether this scenario asks for anything to go wrong.
    ///
    /// Read against what actually landed, because the two come apart and nothing else says so. A
    /// fault is drawn, a packet is lost or not, and a moment written down may fall after the run
    /// has ended — so a scenario can arm plenty and a run can meet none of it. Measured: an
    /// execution client's campaign armed a kill three hundred milliseconds in, ended with its
    /// caller at forty-four, and reported twenty clean runs having pressed on the client not once.
    #[must_use]
    pub fn asks_for_adversity(&self) -> bool {
        let drawn = self.fault_policy.kill.0 > 0
            || self.network.drop_probability.0 > 0
            || self.network.duplicate_probability.0 > 0
            || self.network.reorder_probability.0 > 0
            || self.disk.stall_probability.0 > 0
            || self.disk.eio_probability.0 > 0
            || self.disk.enospc_probability.0 > 0;
        drawn
            || !self.node_faults.is_empty()
            || !self.network.partitions.is_empty()
            || !self.network.grey.is_empty()
    }
}

/// What a scenario says about how long its run is.
///
/// Plain values rather than a `RunConfig`: this crate parses the file because the text names its
/// own types, and a run bound names none of them. It travels as data, and the crate that owns
/// wiring is what turns it into a bound.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RunLimits {
    /// Virtual time at which a guest that was never going to end on its own is done.
    pub horizon: Option<Tick>,
    /// Virtual time after which a guest that has not ended is hung.
    pub max_ticks: Option<Tick>,
    /// The node whose ending is the run's ending, where the scenario names one.
    ///
    /// A cluster of a service and the thing that drives it has no ending of its own, and a horizon
    /// has to be guessed at generously enough to cover whatever the driver does. This says it
    /// exactly, and what it saves is every tick the run would spend watching a server talk to
    /// nobody — most of one, against a client that goes on polling.
    pub ends_with_node: Option<u32>,
}

/// The `[cluster]` section: how many nodes, and what each runs.
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
#[derive(Debug)]
pub struct Cluster {
    /// Node count.
    pub nodes: u32,
    /// Argument vector, with `{node}` replaced by the node's number.
    pub argv: Vec<String>,
    /// One argument vector per node, where the nodes are not alike.
    ///
    /// A cluster of like nodes is one command line with an index substituted, which is most of
    /// them. A cluster of unlike ones — a service and the thing that drives it, a client and the
    /// server it holds to its word — has as many command lines as it has nodes, and there is
    /// nowhere in one list to put the second. This is that place.
    pub programs: Vec<Vec<String>>,
    /// Environment presented to every node.
    pub env: Vec<(String, String)>,
}

impl Cluster {
    /// The argument vector node `index` runs, with the `{node}` token substituted.
    ///
    /// Its own where the scenario gave it one, and the shared template otherwise. The token is
    /// substituted either way: a node that knows its own number needs it whether or not it shares
    /// a command line with anybody.
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
    #[must_use]
    pub fn argv_for(&self, index: u32) -> Vec<String> {
        with_node(self.programs.get(index as usize).unwrap_or(&self.argv), index)
    }
}

impl Implementation {
    /// The argument vector this implementation stands in with, or nothing where it named none.
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
    #[must_use]
    pub fn argv_for(&self) -> Option<Vec<String>> {
        (!self.argv.is_empty()).then(|| with_node(&self.argv, self.node))
    }
}

/// Substitutes `{node}` for the number of the node a command line belongs to.
///
/// One rule for both callers: a node that knows its own number needs it whether it shares a command
/// line with the rest of its cluster or stands in for one node of it.
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
fn with_node(argv: &[String], index: u32) -> Vec<String> {
    argv.iter().map(|a| a.replace("{node}", &index.to_string())).collect()
}

/// The file as serde sees it; conversion and validation happen in [`load`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    run: Option<RawRun>,
    state: Option<RawState>,
    differential: Option<RawDifferential>,
    liveness: Option<RawLiveness>,
    durability: Option<RawDurability>,
    cluster: Option<RawCluster>,
    network: Option<RawNetwork>,
    disk: Option<RawDisk>,
    faults: Option<RawFaults>,
    coverage: Option<RawCoverage>,
    linux: Option<RawLinux>,
    #[serde(default)]
    partition: Vec<RawPartition>,
    #[serde(default)]
    grey: Vec<RawGrey>,
    #[serde(default)]
    fault: Vec<RawFault>,
}

/// The `[run]` section, for the two keys this build can honour.
///
/// The rest of what `docs/API.md` §5.1 shows under `[run]` is named here rather than left to
/// serde's "unknown field", because that message would tell a reader the key does not exist when
/// what it actually needs to say is which flag does the job instead.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRun {
    horizon_ticks: Option<u64>,
    max_ticks: Option<u64>,
    ends_with_node: Option<u32>,
    seed: Option<toml::Value>,
    scheduler: Option<toml::Value>,
    pct_depth: Option<toml::Value>,
    switch_points: Option<toml::Value>,
    budget_branches: Option<toml::Value>,
    timeout_wall_secs: Option<toml::Value>,
}

/// The `[differential]` section: what is compared, and what against.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDifferential {
    projections: Vec<RawProjection>,
    #[serde(default)]
    implementation: Vec<RawImplementation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjection {
    label: String,
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImplementation {
    name: String,
    #[serde(default)]
    argv: Vec<String>,
    node: Option<u32>,
}

/// The `[state]` section: the filesystem a run begins from.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    dir: String,
    digest: Option<String>,
    cwd: Option<String>,
}

/// The `[liveness]` section: how long a healed cluster may commit nothing before that is a finding.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLiveness {
    after_heal_ticks: Option<u64>,
    quiet_ticks: Option<u64>,
    progress: Option<String>,
}

/// The `[durability]` section: what counts as a node answering before it made something durable.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDurability {
    ack: Option<String>,
}

/// The `[faults]` section: whether faults are drawn, and how they choose their moment.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFaults {
    kill_probability: Option<String>,
    targeting: Option<String>,
    restart_after_ticks: Option<u64>,
    arm_after_ticks: Option<u64>,
    budget_per_run: Option<u32>,
    draw: Option<Vec<String>>,
}

/// The `[coverage]` section, for the one key this build can honour.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCoverage {
    shm_size: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCluster {
    nodes: u32,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    programs: Vec<Vec<String>>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    topology: Option<String>,
    latency_mean_ticks: Option<u64>,
    latency_jitter_ticks: Option<u64>,
    drop_probability: Option<String>,
    duplicate_probability: Option<String>,
    reorder_probability: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLinux {
    io_uring: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDisk {
    sync_semantics: Option<String>,
    stall_probability: Option<String>,
    stall_mean_ticks: Option<u64>,
    eio_probability: Option<String>,
    enospc_probability: Option<String>,
    snapshot_depth: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPartition {
    at_tick: u64,
    heal_at_tick: u64,
    cut: RawCut,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCut {
    from: Vec<u32>,
    to: Vec<u32>,
    #[serde(default)]
    directional: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrey {
    link: RawLink,
    at_tick: u64,
    until_tick: u64,
    drop_probability: Option<String>,
    latency_factor: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLink {
    from: u32,
    to: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFault {
    kind: String,
    /// The node this acts on. Absent for a fault that acts on the cluster.
    node: Option<u32>,
    at_tick: u64,
    snapshot_back: Option<usize>,
    /// The nodes a cluster-wide fault rewinds. Absent means every node.
    nodes: Option<Vec<u32>>,
    /// How far a `clock_step` moves the wall clock, in ticks; negative steps it back.
    step_ticks: Option<i64>,
    /// The rate a `clock_drift` sets, in parts per million.
    drift_ppm: Option<i32>,
}

/// Parses and validates a configuration file's text.
/// Parses and validates a configuration file's text.
///
/// # Errors
///
/// Returns the key it refuses and why, so a mistake is fixed by reading the message rather than
/// by bisecting the file.
pub fn load(text: &str) -> Result<LoadedConfig, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| {
        ConfigError::Refused(format!("BOHR-1002: configuration does not parse: {e}"))
    })?;

    let mut network = NetworkConfig::default();
    if let Some(net) = &raw.network {
        network.topology = match net.topology.as_deref() {
            None | Some("full") => Topology::Full,
            Some("ring") => Topology::Ring,
            Some("star") => Topology::Star,
            Some(other) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: network.topology has no layout named {other:?}; \
                     this build knows full, ring, star"
                )));
            }
        };
        network.latency_mean_ticks = net.latency_mean_ticks.unwrap_or(0);
        network.latency_jitter_ticks = net.latency_jitter_ticks.unwrap_or(0);
        network.drop_probability =
            probability(net.drop_probability.as_deref(), "network.drop_probability")?;
        network.duplicate_probability =
            probability(net.duplicate_probability.as_deref(), "network.duplicate_probability")?;
        network.reorder_probability =
            probability(net.reorder_probability.as_deref(), "network.reorder_probability")?;
    }
    if let Some(cluster) = &raw.cluster {
        network.nodes = cluster.nodes;
    }

    for partition in &raw.partition {
        let mut cut = Vec::new();
        for from in &partition.cut.from {
            for to in &partition.cut.to {
                cut.push((NodeId(*from), NodeId(*to)));
                if !partition.cut.directional {
                    cut.push((NodeId(*to), NodeId(*from)));
                }
            }
        }
        network.partitions.push(ScheduledPartition {
            at_tick: Tick(partition.at_tick),
            heal_at_tick: Tick(partition.heal_at_tick),
            cut,
        });
    }

    for grey in &raw.grey {
        network.grey.push(GreyWindow {
            link: (NodeId(grey.link.from), NodeId(grey.link.to)),
            at_tick: Tick(grey.at_tick),
            until_tick: Tick(grey.until_tick),
            drop_probability: match grey.drop_probability.as_deref() {
                Some(text) => Some(probability(Some(text), "grey.drop_probability")?),
                None => None,
            },
            latency_factor: grey.latency_factor,
        });
    }

    let mut disk = DiskConfig::default();
    if let Some(raw_disk) = &raw.disk {
        match raw_disk.sync_semantics.as_deref() {
            None | Some("strict") => {}
            // Named in the format but deliberately refused: answering "lenient" with strict
            // behaviour would be a config that lies.
            Some(other) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: disk.sync_semantics {other:?} is not available; \
                     this build implements strict"
                )));
            }
        }
        disk.stall_probability =
            probability(raw_disk.stall_probability.as_deref(), "disk.stall_probability")?;
        disk.stall_mean_ticks = raw_disk.stall_mean_ticks.unwrap_or(0);
        disk.eio_probability =
            probability(raw_disk.eio_probability.as_deref(), "disk.eio_probability")?;
        disk.enospc_probability =
            probability(raw_disk.enospc_probability.as_deref(), "disk.enospc_probability")?;
        if let Some(depth) = raw_disk.snapshot_depth {
            disk.snapshot_depth = depth;
        }
    }

    let mut fault_policy = FaultPolicy::default();
    if let Some(raw_faults) = &raw.faults {
        fault_policy.kill =
            probability(raw_faults.kill_probability.as_deref(), "faults.kill_probability")?;
        fault_policy.targeting = match raw_faults.targeting.as_deref() {
            None | Some("uniform") => Targeting::Uniform,
            Some("guided") => Targeting::Guided,
            Some(other) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: faults.targeting {other:?} is neither uniform nor guided"
                )));
            }
        };
        fault_policy.restart_after = bohr_types::Tick(raw_faults.restart_after_ticks.unwrap_or(0));
        fault_policy.arm_after = bohr_types::Tick(raw_faults.arm_after_ticks.unwrap_or(0));
        if let Some(budget) = raw_faults.budget_per_run {
            if budget == 0 {
                return Err(ConfigError::Refused(
                    "BOHR-1002: faults.budget_per_run is zero, which arms a policy that can never \
                     fire; leave the key out to run without faults"
                        .to_owned(),
                ));
            }
            fault_policy.budget_per_run = Some(budget);
        }
        if let Some(named) = &raw_faults.draw {
            // Named rather than defaulted to everything: a campaign that drew a freeze it never
            // asked for would be searching something its scenario does not describe, and the
            // kinds it can search are what its bundle has to be read against.
            let mut kinds = crate::DrawnFaults {
                kill: false,
                freeze: false,
                isolate: false,
                degrade: false,
                recover_from_snapshot: false,
                clock_step: false,
                clock_drift: false,
            };
            for name in named {
                match name.as_str() {
                    "node_kill" => kinds.kill = true,
                    "node_freeze" => kinds.freeze = true,
                    "node_isolate" => kinds.isolate = true,
                    "node_degrade" => kinds.degrade = true,
                    // Named as a recovery rather than as a kind, because that is what it is: the
                    // fault is still the kill and what changes is the disk the node comes back on.
                    "recover_from_snapshot" => kinds.recover_from_snapshot = true,
                    "clock_step" => kinds.clock_step = true,
                    "clock_drift" => kinds.clock_drift = true,
                    other => {
                        return Err(ConfigError::Refused(format!(
                            "BOHR-1002: faults.draw has nothing named {other:?}; this build \
                             draws node_kill, node_freeze, node_isolate, node_degrade, \
                             clock_step and clock_drift, and takes recover_from_snapshot for what a killed node comes back on. \
                             A thaw, a heal or a restart is not drawn: it is how a node comes \
                             back from the fault that took it away"
                        )));
                    }
                }
            }
            if !kinds.kill
                && !kinds.freeze
                && !kinds.isolate
                && !kinds.degrade
                && !kinds.clock_step
                && !kinds.clock_drift
            {
                return Err(ConfigError::Refused(
                    "BOHR-1002: faults.draw names no kind to draw, so the policy could do nothing"
                        .to_owned(),
                ));
            }
            fault_policy.kinds = kinds;
        }
    }

    let mut node_faults = Vec::new();
    for fault in &raw.fault {
        let kind = match fault.kind.as_str() {
            "node_kill" => NodeFaultKind::Kill,
            "node_freeze" => NodeFaultKind::Freeze,
            "node_thaw" => NodeFaultKind::Thaw,
            "node_restart" => NodeFaultKind::Restart,
            "node_restart_from_snapshot" => NodeFaultKind::RestartFromSnapshot {
                back: fault.snapshot_back.ok_or_else(|| {
                    ConfigError::Refused(
                        "BOHR-1002: node_restart_from_snapshot needs snapshot_back".to_owned(),
                    )
                })?,
            },
            // In the existing `[[fault]]` table with the existing `at_tick` key, deliberately: a
            // table of its own would be invisible to reduction, and nobody would notice until the
            // first real finding refused to shrink.
            "chain_reorg" => NodeFaultKind::ChainReorg {
                back: fault.snapshot_back.ok_or_else(|| {
                    ConfigError::Refused(
                        "BOHR-1002: chain_reorg needs snapshot_back, which is how far back the \
                         common ancestor is"
                            .to_owned(),
                    )
                })?,
                nodes: fault.nodes.clone().unwrap_or_default().into_iter().map(NodeId).collect(),
            },
            "clock_step" => NodeFaultKind::ClockStep {
                by_ticks: fault.step_ticks.ok_or_else(|| {
                    ConfigError::Refused(
                        "BOHR-1002: clock_step needs step_ticks, negative to step back".to_owned(),
                    )
                })?,
            },
            "clock_drift" => {
                let ppm = fault.drift_ppm.ok_or_else(|| {
                    ConfigError::Refused("BOHR-1002: clock_drift needs drift_ppm".to_owned())
                })?;
                // Refused here rather than by the clock's own assertion: a scenario with a typo
                // in it is a refusal at load, not a run that panics at the fault.
                if ppm.unsigned_abs() > bohr_kernel::MAX_DRIFT_PPM {
                    return Err(ConfigError::Refused(format!(
                        "BOHR-1002: clock_drift's drift_ppm is {ppm}, and a clock may drift at \
                         most {} parts per million either way",
                        bohr_kernel::MAX_DRIFT_PPM
                    )));
                }
                NodeFaultKind::ClockDrift { ppm }
            }
            other => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: fault.kind has no fault named {other:?}"
                )));
            }
        };
        // A clock's key on another kind would be read as meaning something, and it means nothing.
        let stray = match kind {
            NodeFaultKind::ClockStep { .. } => fault.drift_ppm.map(|_| "drift_ppm"),
            NodeFaultKind::ClockDrift { .. } => fault.step_ticks.map(|_| "step_ticks"),
            _ => fault.step_ticks.map(|_| "step_ticks").or(fault.drift_ppm.map(|_| "drift_ppm")),
        };
        if let Some(key) = stray {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: fault.{key} does nothing on a {} fault",
                fault.kind
            )));
        }
        // A cluster-wide fault names no node, and a node fault must: a `node` on a reorg would be
        // read as meaning something, and it means nothing.
        let cluster_wide = matches!(kind, NodeFaultKind::ChainReorg { .. });
        let node = match (cluster_wide, fault.node) {
            (false, Some(node)) => NodeId(node),
            (false, None) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: fault.kind {:?} acts on one node and this entry names none",
                    fault.kind
                )));
            }
            (true, None) => NodeId::ZERO,
            (true, Some(_)) => return Err(ConfigError::Refused(
                "BOHR-1002: chain_reorg acts on the cluster, so it takes nodes rather than node"
                    .to_owned(),
            )),
        };
        // **A node the cluster does not have is a typo.** The world creates a node it is asked
        // about, so `node = 5` in a cluster of three would produce a fourth to kill and restart --
        // a scenario that runs, reports nothing and tests nothing anybody meant. §8 rule 6:
        // refused where it is read.
        let known = network.nodes;
        let named: Vec<u32> = match &kind {
            NodeFaultKind::ChainReorg { nodes, .. } => {
                nodes.iter().map(|node| node.get()).collect()
            }
            _ => vec![node.get()],
        };
        if let Some(missing) = named.into_iter().find(|named| *named >= known) {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: a fault names node {missing}, and this cluster has {known}"
            )));
        }
        node_faults.push(ScheduledNodeFault { at_tick: Tick(fault.at_tick), node, kind });
    }

    let cluster = match raw.cluster {
        None => None,
        Some(c) => {
            // Neither is required: a scenario that names no program at all takes the one given on
            // the command line, which is how every single-program scenario here is written. What
            // is refused is naming both, which leaves a reader to work out which won, and naming
            // fewer programs than there are nodes, which would quietly give a node its
            // neighbour's command line.
            if !c.argv.is_empty() && !c.programs.is_empty() {
                return Err(ConfigError::Refused(
                    "BOHR-1002: [cluster] takes argv or programs, not both".to_owned(),
                ));
            }
            if !c.programs.is_empty() && c.programs.len() != c.nodes as usize {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: [cluster] names {} programs for {} nodes",
                    c.programs.len(),
                    c.nodes
                )));
            }
            Some(Cluster {
                nodes: c.nodes,
                argv: c.argv,
                programs: c.programs,
                env: c.env.into_iter().collect(),
            })
        }
    };

    let run = run_limits(raw.run.as_ref(), cluster.as_ref().map(|c| c.nodes))?;
    let (state, cwd) = state(raw.state.as_ref())?;
    let differential =
        differential(raw.differential.as_ref(), raw.faults.as_ref(), cluster.as_ref())?;
    let liveness = liveness(raw.liveness.as_ref(), &raw.partition)?;
    let durability = durability(raw.durability.as_ref(), raw.faults.as_ref(), &raw.fault)?;

    // A policy armed after the run is over is a policy that never fires, and a configuration that
    // is read and does nothing is a configuration that lies. Both numbers are named, because the
    // reader is about to change one of them and needs to know which.
    if let Some(horizon) = run.horizon
        && fault_policy.arm_after >= horizon
    {
        return Err(ConfigError::Refused(format!(
            "BOHR-1002: faults.arm_after_ticks is {} and run.horizon_ticks is {}, so the run \
                 would end before a drawn fault was ever offered",
            fault_policy.arm_after.get(),
            horizon.get()
        )));
    }

    Ok(LoadedConfig {
        cluster,
        network,
        disk,
        node_faults,
        fault_policy,
        run,
        liveness,
        durability,
        state,
        cwd,
        differential,
        coverage_bytes: coverage(raw.coverage.as_ref())?,
        linux: linux(raw.linux.as_ref(), &disk)?,
    })
}

/// Reads `[linux]`, refusing a ring the disk model could not keep its promises through.
///
/// **A stalled sync inside a ring has no moment to finish.** A plain `fsync` that stalls parks its
/// task and is released by a timer; a submission is carried out inside one `io_uring_enter` that
/// returns with every completion posted, so a drawn stall there would have to be dropped or turned
/// into a refusal at a seed nobody chose. Refused here instead, once, before anything runs.
fn linux(raw: Option<&RawLinux>, disk: &DiskConfig) -> Result<LinuxConfig, ConfigError> {
    let io_uring = match raw.and_then(|l| l.io_uring.as_deref()) {
        None | Some("absent") => IoUring::Absent,
        Some("ring") => IoUring::Ring,
        Some(other) => {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: linux.io_uring {other:?} is neither absent nor ring"
            )));
        }
    };
    if io_uring == IoUring::Ring && disk.stall_probability.0 > 0 {
        return Err(ConfigError::Refused(
            "BOHR-1002: linux.io_uring = \"ring\" with disk.stall_probability above zero is not \
             modelled: a sync stalled inside a submission has no moment to complete"
                .to_owned(),
        ));
    }
    Ok(LinuxConfig { io_uring })
}

/// Reads `[coverage]`, refusing a map that could not tell anything apart or could not be held.
///
/// **A map is copied out of shared memory once per run and held once per worker**, so its size is
/// a cost a campaign pays continuously rather than a number to set generously. The upper bound is
/// where that cost stops being small; the lower one is where the map stops being a map.
fn coverage(raw: Option<&RawCoverage>) -> Result<Option<usize>, ConfigError> {
    let Some(size) = raw.and_then(|c| c.shm_size) else {
        return Ok(None);
    };
    if !(MIN_COVERAGE_BYTES..=MAX_COVERAGE_BYTES).contains(&size) {
        return Err(ConfigError::Refused(format!(
            "BOHR-1002: coverage.shm_size is {size}, and this build holds between \
             {MIN_COVERAGE_BYTES} and {MAX_COVERAGE_BYTES} counters. A map is copied once per run \
             and held once per worker, so its size is a cost the whole campaign pays"
        )));
    }
    Ok(Some(size))
}

/// Reads `[liveness]`, refusing a watch that could never fire.
fn liveness(
    raw: Option<&RawLiveness>,
    partitions: &[RawPartition],
) -> Result<Option<LivenessConfig>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    // Named rather than accepted and ignored. A cluster that has stopped committing still gossips,
    // so counting delivered packets as progress would read a dead cluster as a healthy one for as
    // long as anybody cared to watch.
    match raw.progress.as_deref() {
        None | Some("fsync") => {}
        Some(other) => {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: liveness.progress has no definition named {other:?}; this build counts \
                 a completed fsync and nothing else, because a cluster that has stopped committing \
                 still answers heartbeats"
            )));
        }
    }
    if raw.after_heal_ticks.is_none() && raw.quiet_ticks.is_none() {
        return Err(ConfigError::Refused(
            "BOHR-1002: [liveness] watches for nothing; it takes after_heal_ticks, quiet_ticks, \
             or both"
                .to_owned(),
        ));
    }
    for (key, value) in
        [("after_heal_ticks", raw.after_heal_ticks), ("quiet_ticks", raw.quiet_ticks)]
    {
        if value == Some(0) {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: liveness.{key} is zero, which asks about a span of no time at all"
            )));
        }
    }
    // A watch armed by a heal, in a scenario where nothing is ever cut, never fires — and a
    // configuration that is read and does nothing is a configuration that lies. Only that half of
    // the section is refused: watching for a node that stopped needs nothing to have been cut.
    if raw.after_heal_ticks.is_some() && partitions.is_empty() {
        return Err(ConfigError::Refused(
            "BOHR-1002: liveness.after_heal_ticks watches for a silence after a heal, and this \
             scenario has no [[partition]] to heal"
                .to_owned(),
        ));
    }

    Ok(Some(LivenessConfig {
        after_heal_ticks: Tick(raw.after_heal_ticks.unwrap_or(0)),
        quiet_ticks: Tick(raw.quiet_ticks.unwrap_or(0)),
    }))
}

/// Splits a leading node number off the rest of a projection source.
///
/// `file:1:/head` is node one's; `file:/head` is node zero's, which is what every source written
/// before a node could be named meant and goes on meaning. A path this world can hold is absolute,
/// so what precedes the first colon is a number or it is not there.
fn node_prefix(source: &str) -> (u32, &str) {
    match source.split_once(':') {
        Some((head, rest)) => head.parse().map_or((0, source), |node| (node, rest)),
        None => (0, source),
    }
}

/// Reads `[differential]`, refusing a comparison that could not mean anything.
fn differential(
    raw: Option<&RawDifferential>,
    faults: Option<&RawFaults>,
    cluster: Option<&Cluster>,
) -> Result<Option<Differential>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    if raw.implementation.len() < 2 {
        return Err(ConfigError::Refused(format!(
            "BOHR-1002: a differential run compares implementations against each other, and this \
             scenario names {}",
            raw.implementation.len()
        )));
    }
    if raw.projections.is_empty() {
        return Err(ConfigError::Refused(
            "BOHR-1002: [differential] names no projections, so there is nothing to compare and a \
             run would report agreement about nothing"
                .to_owned(),
        ));
    }
    // Drawn faults draw at every syscall, deliberately, so byte consumption cannot depend on disk
    // state — and two implementations make a different number of syscalls in a different order, so
    // a drawn schedule cannot line up across them under any construction. Scheduled faults can and
    // do. See `docs/ARCHITECTURE.md` §5.20.
    if faults.and_then(|f| f.kill_probability.as_deref()).is_some() {
        return Err(ConfigError::Refused(
            "BOHR-1002: faults.kill_probability draws per syscall, and two implementations make \
             different syscalls, so a drawn fault schedule cannot be the same for both; use a \
             [[fault]] at a tick, which is"
                .to_owned(),
        ));
    }

    // **Which node an implementation is.** A cluster of unlike nodes puts the thing under
    // comparison beside the caller that drives it, and only one of the two is the implementation.
    // Naming a node the cluster has not got would replace nobody, and naming different ones would
    // compare a client against a caller.
    let nodes = cluster.map_or(1, |c| c.nodes.max(1));
    let mut stands_in_for: Option<(&str, u32)> = None;
    for implementation in &raw.implementation {
        let node = implementation.node.unwrap_or(0);
        if node >= nodes {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: implementation {:?} stands in for node {node}, and this scenario runs \
                 {nodes}",
                implementation.name
            )));
        }
        match stands_in_for {
            Some((first, at)) if at != node => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: implementation {first:?} stands in for node {at} and {:?} for node \
                     {node}, so the comparison would be between unlike programs rather than \
                     between two of one",
                    implementation.name
                )));
            }
            _ => stands_in_for = Some((&implementation.name, node)),
        }
    }

    let mut projections = Vec::new();
    for projection in &raw.projections {
        let read = match projection.source.split_once(':') {
            Some(("file", rest)) => {
                let (node, path) = node_prefix(rest);
                Some((node, Source::File(path.to_owned())))
            }
            Some(("stdout", node)) => node.parse().ok().map(|node| (node, Source::Stdout)),
            None if projection.source == "stdout" => Some((0, Source::Stdout)),
            _ => None,
        };
        let Some((node, source)) = read else {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: projection {:?} has no source named {:?}; this build reads \
                 file:<path>, file:<node>:<path>, stdout and stdout:<node>",
                projection.label, projection.source
            )));
        };
        if node >= nodes {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: projection {:?} reads node {node}, and this scenario runs {nodes}",
                projection.label
            )));
        }
        projections.push(ProjectionSpec { label: projection.label.clone(), node, source });
    }

    Ok(Some(Differential {
        implementations: raw
            .implementation
            .iter()
            .map(|i| Implementation {
                name: i.name.clone(),
                argv: i.argv.clone(),
                node: i.node.unwrap_or(0),
            })
            .collect(),
        projections,
    }))
}

/// Reads `[state]`, and checks the pin where the file carries one.
fn state(raw: Option<&RawState>) -> Result<(Option<StateImage>, Option<String>), ConfigError> {
    let Some(raw) = raw else {
        return Ok((None, None));
    };

    let image = StateImage::from_dir(std::path::Path::new(&raw.dir), MAX_STATE_FILE_BYTES)
        .map_err(|e| ConfigError::Refused(e.to_string()))?;

    // A pin is what makes a scenario file say which state it means rather than which directory it
    // happened to find. Checked here, at load, because everything after this point would be a run
    // against a state nobody chose.
    if let Some(pinned) = raw.digest.as_deref() {
        let actual = image.digest().to_hex();
        if pinned.trim_start_matches("blake3:") != actual {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: state.digest names {pinned}, and {} holds {actual}",
                raw.dir
            )));
        }
    }

    Ok((Some(image), raw.cwd.clone()))
}

/// Reads `[durability]`, refusing a watch nothing could ever trip.
fn durability(
    raw: Option<&RawDurability>,
    faults: Option<&RawFaults>,
    scheduled: &[RawFault],
) -> Result<Option<DurabilityConfig>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    // Named rather than accepted and ignored, because it is an approximation and the reader has to
    // know which one: this model has no word for an acknowledgement, so what stands in for one is
    // the moment a node reached outward with unsynced writes behind it.
    match raw.ack.as_deref() {
        None | Some("send_while_dirty") => {}
        Some(other) => {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: durability.ack has no definition named {other:?}; this build counts an \
                 outbound datagram sent while the node's disk is dirty, because the world has no \
                 word for an acknowledgement and reading the guest's protocol is out of scope"
            )));
        }
    }
    // The finding needs a node to come back without something, so a scenario where nothing can
    // kill one can never produce it — and a configuration that is read and does nothing lies.
    let can_kill = faults.and_then(|f| f.kill_probability.as_deref()).is_some()
        || scheduled.iter().any(|f| f.kind == "node_kill");
    if !can_kill {
        return Err(ConfigError::Refused(
            "BOHR-1002: [durability] reports what a restarted node came back without, and this \
             scenario has neither faults.kill_probability nor a [[fault]] that kills one"
                .to_owned(),
        ));
    }

    Ok(Some(DurabilityConfig))
}

/// Reads `[run]`, refusing the keys this build cannot honour by name.
fn run_limits(raw: Option<&RawRun>, nodes: Option<u32>) -> Result<RunLimits, ConfigError> {
    let Some(raw) = raw else {
        return Ok(RunLimits::default());
    };

    for (key, flag) in [
        ("seed", raw.seed.is_some()),
        ("scheduler", raw.scheduler.is_some()),
        ("pct_depth", raw.pct_depth.is_some()),
        ("switch_points", raw.switch_points.is_some()),
        ("budget_branches", raw.budget_branches.is_some()),
        ("timeout_wall_secs", raw.timeout_wall_secs.is_some()),
    ] {
        if flag {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: run.{key} is not read from a scenario; \
                 it belongs to the invocation, so a file setting it would be a file that lies"
            )));
        }
    }

    let horizon = raw.horizon_ticks.map(Tick);
    let max_ticks = raw.max_ticks.map(Tick);

    if horizon == Some(Tick(0)) {
        return Err(ConfigError::Refused(
            "BOHR-1002: run.horizon_ticks is zero, which completes a run that never ran".to_owned(),
        ));
    }
    // A horizon the budget reaches first never fires, and a configuration that is read and does
    // nothing is a configuration that lies. Refused with both numbers, because the reader is about
    // to change one of them and needs to know which.
    if let (Some(at), Some(budget)) = (horizon, max_ticks)
        && at >= budget
    {
        return Err(ConfigError::Refused(format!(
            "BOHR-1002: run.horizon_ticks is {} and run.max_ticks is {}, so the budget would \
                 end the run first and the horizon would never fire",
            at.get(),
            budget.get()
        )));
    }

    // Refused rather than clamped, because a node that is not in the cluster never ends and the
    // run would quietly wait out its horizon instead — a key that is read and does nothing.
    if let Some(node) = raw.ends_with_node
        && nodes.is_some_and(|count| node >= count)
    {
        return Err(ConfigError::Refused(format!(
            "BOHR-1002: run.ends_with_node is {node} and the cluster has {} nodes, so the run \
                 would wait for a node that is not there",
            nodes.unwrap_or(0)
        )));
    }

    Ok(RunLimits { horizon, max_ticks, ends_with_node: raw.ends_with_node })
}

/// Parses `"num/den"`, refusing what the boolean draw cannot express.
fn probability(text: Option<&str>, key: &str) -> Result<(u32, u32), ConfigError> {
    let Some(text) = text else {
        return Ok((0, 1));
    };
    let parsed = text.split_once('/').and_then(|(num, den)| {
        Some((num.trim().parse::<u32>().ok()?, den.trim().parse::<u32>().ok()?))
    });
    let Some((num, den)) = parsed else {
        return Err(ConfigError::Refused(format!(
            "BOHR-1002: {key} is not a rational: {text:?}; probabilities are written \"1/1000\""
        )));
    };
    if den == 0 || den > MAX_DENOMINATOR {
        // The nearest expressible value is part of the refusal so the fix is a copy-paste.
        return Err(ConfigError::Refused(format!(
            "BOHR-1003: {key} denominator {den} is outside 1..={MAX_DENOMINATOR}; \
             nearest expressible: {num}/{MAX_DENOMINATOR}"
        )));
    }
    // **A probability above one is not a probability.** Accepted, it read as certainty -- and a
    // certainty costs no tape, so a typed `1000/100` silently changed how every later draw in the
    // run was laid out as well as making the thing it named inevitable.
    if num > den {
        return Err(ConfigError::Refused(format!(
            "BOHR-1003: {key} is {num}/{den}, which is more than certain"
        )));
    }
    Ok((num, den))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_scenario_parses_into_the_world_types() {
        let loaded = load(
            r#"
            [cluster]
            nodes = 5
            argv = ["./raft-node", "{node}"]

            [network]
            topology = "full"
            latency_mean_ticks = 500_000
            drop_probability = "1/1000"

            [disk]
            stall_probability = "1/500"
            stall_mean_ticks = 5_000_000
            snapshot_depth = 2

            [[partition]]
            at_tick = 5_000_000
            heal_at_tick = 9_000_000
            cut = { from = [0, 1], to = [2, 3, 4], directional = true }

            [[fault]]
            kind = "node_kill"
            node = 1
            at_tick = 3_000_000
            "#,
        )
        .expect("a well-formed configuration loads");

        let cluster = loaded.cluster.expect("the cluster section");
        assert_eq!(cluster.nodes, 5);
        assert_eq!(cluster.argv_for(3), ["./raft-node", "3"]);
        assert_eq!(loaded.network.drop_probability, (1, 1000));
        assert_eq!(loaded.network.partitions.len(), 1);
        assert_eq!(loaded.network.partitions[0].cut.len(), 6, "2 x 3 directed pairs");
        assert_eq!(loaded.disk.snapshot_depth, 2);
        assert_eq!(loaded.node_faults.len(), 1);
    }

    #[test]
    fn a_symmetric_cut_lists_both_directions() {
        let loaded = load(
            r#"
            [[partition]]
            at_tick = 1
            heal_at_tick = 2
            cut = { from = [0], to = [1] }
            "#,
        )
        .expect("loads");
        assert_eq!(loaded.network.partitions[0].cut.len(), 2, "undirected cuts go both ways");
    }

    /// A map has to be big enough to tell paths apart and small enough to copy once per run.
    ///
    /// Both bounds are refusals rather than clamps, for the reason every other value here is: a
    /// campaign that asked for a size and silently got another would report coverage numbers
    /// against a map nobody chose.
    #[test]
    fn a_coverage_map_too_small_to_read_or_too_large_to_hold_is_refused() {
        let sized = |bytes: &str| {
            load(&format!(
                "[coverage]
shm_size = {bytes}
"
            ))
        };

        let held = sized("262144").expect("a quarter of a megabyte is a map");
        assert_eq!(held.coverage_bytes, Some(262_144));
        assert!(sized("16").is_err(), "a map of sixteen counters was accepted");
        assert!(sized("134217728").is_err(), "a map of 128 MiB was accepted");
        assert!(
            load(
                "[coverage]
mode = \"shm\"
"
            )
            .is_err(),
            "a key this build does not implement was accepted"
        );
    }

    #[test]
    fn a_denominator_past_the_draw_is_refused_with_the_nearest_value() {
        let err = load("[network]\ndrop_probability = \"1/100000\"\n").expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("BOHR-1003") && text.contains("1/65536"), "got: {text}");
    }

    #[test]
    fn an_unknown_key_is_refused_not_skipped() {
        assert!(load("[network]\nlatencccy = 5\n").is_err(), "silent skipping hides typos");
    }

    #[test]
    fn lenient_disk_semantics_are_refused_until_they_exist() {
        let err = load("[disk]\nsync_semantics = \"lenient\"\n").expect_err("refused");
        assert!(err.to_string().contains("strict"));
    }

    /// The ring is asked for by name, absent by default, and a third value is a typo.
    #[test]
    fn a_ring_is_named_or_absent() {
        assert_eq!(load("").unwrap().linux.io_uring, IoUring::Absent);
        assert_eq!(
            load("[linux]\nio_uring = \"absent\"\n").unwrap().linux.io_uring,
            IoUring::Absent
        );
        assert_eq!(load("[linux]\nio_uring = \"ring\"\n").unwrap().linux.io_uring, IoUring::Ring);
        let err = load("[linux]\nio_uring = \"sqpoll\"\n").expect_err("refused");
        assert!(err.to_string().contains("BOHR-1002"), "{err}");
    }

    /// A stalled sync inside a ring has no moment to finish, so the pair is refused before the run.
    #[test]
    fn a_ring_with_stalling_syncs_is_refused() {
        let err = load("[linux]\nio_uring = \"ring\"\n[disk]\nstall_probability = \"1/10\"\n")
            .expect_err("refused");
        assert!(err.to_string().contains("stall"), "{err}");
        assert!(load("[disk]\nstall_probability = \"1/10\"\n").is_ok(), "without a ring it runs");
    }

    /// An implementation stands in for one node of the cluster, and which one is part of the
    /// comparison rather than a detail: a client and the caller that drives it are not two
    /// implementations of one thing.
    #[test]
    fn an_implementation_stands_in_for_a_node_the_cluster_has() {
        let compared = |first: &str, second: &str| {
            load(&format!(
                r#"
                [cluster]
                nodes = 2
                programs = [["./client"], ["./caller"]]

                [differential]
                projections = [{{ label = "answer", source = "file:/answer" }}]

                [[differential.implementation]]
                name = "reference"
                argv = ["./reference"]
                {first}

                [[differential.implementation]]
                name = "second"
                argv = ["./second"]
                {second}
                "#
            ))
        };

        let loaded = compared("node = 1", "node = 1").expect("a node the cluster has");
        let differential = loaded.differential.expect("the differential section");
        assert_eq!(differential.implementations[0].node, 1);
        assert_eq!(
            differential.implementations[0].argv_for(),
            Some(vec!["./reference".to_owned()])
        );

        let unnamed = compared("", "").expect("no node named is node zero");
        assert_eq!(unnamed.differential.expect("the section").implementations[1].node, 0);

        let absent = compared("node = 2", "node = 2").expect_err("a node the cluster has not got");
        assert!(absent.to_string().contains("BOHR-1002"), "got: {absent}");

        let unlike = compared("node = 0", "node = 1").expect_err("a client against its caller");
        assert!(unlike.to_string().contains("unlike programs"), "got: {unlike}");
    }

    /// A source says which node holds the value, and a source written before it could says node
    /// zero — which is what it always meant.
    #[test]
    fn a_projection_source_carries_the_node_that_holds_it() {
        let read = |source: &str| {
            load(&format!(
                r#"
                [cluster]
                nodes = 2

                [differential]
                projections = [{{ label = "answer", source = "{source}" }}]

                [[differential.implementation]]
                name = "reference"
                argv = ["./reference"]

                [[differential.implementation]]
                name = "second"
                argv = ["./second"]
                "#
            ))
            .map(|loaded| {
                let spec = loaded.differential.expect("the section").projections.remove(0);
                (spec.node, spec.source)
            })
        };

        assert_eq!(read("file:/answer").expect("a path"), (0, Source::File("/answer".to_owned())));
        assert_eq!(
            read("file:1:/answer").expect("a node and a path"),
            (1, Source::File("/answer".to_owned()))
        );
        assert_eq!(read("stdout").expect("output"), (0, Source::Stdout));
        assert_eq!(read("stdout:1").expect("a node's output"), (1, Source::Stdout));

        let absent = read("file:2:/answer").expect_err("a node the cluster has not got");
        assert!(absent.to_string().contains("BOHR-1002"), "got: {absent}");
    }
}
