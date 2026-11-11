//! The world configuration file: `bohr run --config cluster.toml`.
//!
//! The shape is normative in `docs/API.md` §5. Everything is validated at load, and every refusal
//! names its key.
//!
//! It lives beside the models it configures, shared by the binary and the reduction harness.

use bohr_types::{NodeId, Tick};
use serde::Deserialize;

use crate::{
    DiskConfig, DurabilityConfig, FaultPolicy, GreyWindow, LivenessConfig, NetworkConfig,
    NodeFaultKind, ProjectionSpec, ScheduledNodeFault, ScheduledPartition, Source, StateImage,
    Targeting, Topology,
    probe::{ProbeSpec, Property},
};

/// Why a configuration was refused.
///
/// One variant, since callers react to every refusal the same way.
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
    /// The cluster shape, when one is configured. Builds without a backend validate it, then
    /// refuse.
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
    /// The filesystem every node starts from, where the scenario prepared one and the caller read
    /// it from [`Self::state_source`].
    pub state: Option<StateImage>,
    /// Where the prepared state is on the host, and the digest it is pinned to, as the scenario
    /// names them.
    pub state_source: Option<StateSource>,
    /// What `getcwd` answers, where the scenario says.
    pub cwd: Option<String>,
    /// The implementations a differential run compares, and the values it compares them by.
    pub differential: Option<Differential>,
    /// Counters in the coverage map, where the scenario sizes one.
    pub coverage_bytes: Option<usize>,
    /// The kernel the guest is shown, where the scenario names one other than the default.
    pub linux: LinuxConfig,
    /// What the probe asks the nodes, and what their answers must keep.
    ///
    /// Empty where the scenario asks nothing, and then no probe node is started.
    pub probes: Vec<ProbeSpec>,
    /// The SDK fault points a run may arm, by the names the guest gives them.
    pub fault_points: Vec<String>,
    /// The odds each listed fault point is armed on a node, drawn once per run.
    pub fault_point_odds: (u32, u32),
    /// The family whose laws judge the run, where the scenario names one.
    pub model: Option<crate::model::ModelConfig>,
    /// What the load sends the chain, where the scenario starts one.
    pub workload: Option<crate::adapter::Workload>,
}

/// The `[linux]` section: which kernel facilities the guest is told exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LinuxConfig {
    /// Whether opening a submission ring succeeds.
    pub io_uring: IoUring,
}

/// What `io_uring_setup` is answered.
///
/// `Absent`, the default, answers `ENOSYS` like a kernel without io_uring; programs keep a path for
/// it. `Ring` carries file operations through the same model as plain calls and refuses the rest.
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
    /// Empty means the node's usual program.
    pub argv: Vec<String>,
    /// Which node's program this stands in for.
    ///
    /// A real client is driven by a caller on another node; only this node is replaced.
    pub node: u32,
}

impl LoadedConfig {
    /// What this scenario asks to be judged on.
    #[must_use]
    pub fn watching(&self) -> crate::Watching {
        let has = |p: Property| self.probes.iter().any(|s| s.property == p);
        crate::Watching {
            liveness: self.liveness.is_some(),
            durability: self.durability.is_some(),
            laws: self.model.is_some(),
            probed: crate::Probed {
                never_reverts: has(Property::NeverReverts),
                monotonic: has(Property::Monotonic),
                agrees: has(Property::Agrees),
                advances: has(Property::Advances),
                unique: has(Property::Unique),
            },
        }
    }

    /// The node the load runs on: the one after the scenario's own, where a workload starts one.
    #[must_use]
    pub fn load_node(&self) -> Option<u32> {
        self.workload.as_ref().map(|_| self.scenario_nodes())
    }

    /// The node the probe runs on: after the scenario's own nodes and the load.
    #[must_use]
    pub fn probe_node(&self) -> u32 {
        self.scenario_nodes() + u32::from(self.workload.is_some())
    }

    fn scenario_nodes(&self) -> u32 {
        self.cluster.as_ref().map_or(1, |c| c.nodes.max(1))
    }

    /// Whether this scenario asks for anything to go wrong.
    ///
    /// Compared with what landed: a run can end before any armed fault fires.
    #[must_use]
    pub fn asks_for_adversity(&self) -> bool {
        let drawn = self.fault_policy.kill.0 > 0
            || self.fault_policy.enomem.0 > 0
            || self.network.drop_probability.0 > 0
            || self.network.duplicate_probability.0 > 0
            || self.network.reorder_probability.0 > 0
            || self.network.corrupt_probability.0 > 0
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
/// Plain values; the wiring crate turns them into a `RunConfig`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RunLimits {
    /// Virtual time at which a guest that was never going to end on its own is done.
    pub horizon: Option<Tick>,
    /// Virtual time after which a guest that has not ended is hung.
    pub max_ticks: Option<Tick>,
    /// The node whose ending is the run's ending, where the scenario names one.
    ///
    /// Ends the run with the driver instead of a guessed horizon.
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
    /// For clusters of unlike nodes, such as a service and its driver.
    pub programs: Vec<Vec<String>>,
    /// Environment presented to every node.
    pub env: Vec<(String, String)>,
    /// Each node's role, one of [`ROLES`]; empty where every node is a target.
    pub roles: Vec<String>,
}

impl Cluster {
    /// The role of node `index`: `target` unless the scenario names another.
    #[must_use]
    pub fn role_of(&self, index: u32) -> &str {
        self.roles.get(index as usize).map_or("target", String::as_str)
    }

    /// The argument vector node `index` runs, with the `{node}` token substituted.
    ///
    /// Its own argv if given, else the shared template; `{node}` is substituted in both.
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
/// Shared by cluster and differential command lines.
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
    chain: Option<RawChain>,
    model: Option<RawModel>,
    adapter: Option<crate::adapter::RawAdapter>,
    workload: Option<crate::adapter::RawWorkload>,
    #[serde(default)]
    probe: Vec<RawProbe>,
    #[serde(default)]
    partition: Vec<RawPartition>,
    #[serde(default)]
    grey: Vec<RawGrey>,
    #[serde(default)]
    fault: Vec<RawFault>,
}

/// The `[run]` section, for the two keys this build can honour.
///
/// Other `[run]` keys in `docs/API.md` §5.1 are refused with the flag that replaces them.
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

/// The `[model]` section: which family's laws judge the run.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    kind: String,
    ready_within_ticks: Option<u64>,
    moves_within_ticks: Option<u64>,
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
    enomem_probability: Option<String>,
    targeting: Option<String>,
    restart_after_ticks: Option<u64>,
    arm_after_ticks: Option<u64>,
    disarm_after_ticks: Option<u64>,
    ablate: Option<Vec<String>>,
    budget_per_run: Option<u32>,
    draw: Option<Vec<String>>,
    fault_points: Option<Vec<String>>,
    fault_point_probability: Option<String>,
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
    #[serde(default)]
    roles: Vec<String>,
}

/// What a node is to a run: the system under test, or part of the bench around it.
///
/// A finding whose failing nodes are all bench is the bench's, never the target's.
pub const ROLES: [&str; 3] = ["target", "load", "driver"];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    topology: Option<String>,
    latency_mean_ticks: Option<u64>,
    latency_jitter_ticks: Option<u64>,
    drop_probability: Option<String>,
    duplicate_probability: Option<String>,
    reorder_probability: Option<String>,
    corrupt_probability: Option<String>,
    corrupt_bytes: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLinux {
    io_uring: Option<String>,
}

/// The `[chain]` section: the blockchain oracles, each a probe written out in advance.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChain {
    oracles: Vec<String>,
    nodes: Option<Vec<u32>>,
    port: Option<u16>,
    every_ticks: u64,
    stall_ticks: Option<u64>,
    votes_rpc: Option<String>,
}

/// One `[[probe]]`: a question asked of some nodes, and the property its answers keep.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProbe {
    name: Option<String>,
    node: Option<u32>,
    nodes: Option<Vec<u32>>,
    port: Option<u16>,
    rpc: String,
    params: Option<toml::Value>,
    every_ticks: u64,
    key: Option<toml::Value>,
    value: String,
    property: String,
    within_ticks: Option<u64>,
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
    reorder_unsynced: Option<bool>,
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

/// The most nodes a cluster may have.
pub const MAX_NODES: u32 = 1024;

/// The keys of a scenario that name a moment the world acts at, which a reduction may bring
/// forward.
///
/// Listed by name: `latency_mean_ticks` is also ticks, but lowering it makes a different scenario.
pub const MOMENT_KEYS: [&str; 4] = ["at_tick", "heal_at_tick", "until_tick", "restart_after_ticks"];

/// Parses and validates a configuration file's text.
///
/// Pure: a `[state]` is named in [`LoadedConfig::state_source`] and left unread, for the caller
/// that may read the host to fill [`LoadedConfig::state`].
///
/// # Errors
///
/// Returns the key it refuses and why.
pub fn parse(text: &str) -> Result<LoadedConfig, ConfigError> {
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
        network.corrupt_probability =
            probability(net.corrupt_probability.as_deref(), "network.corrupt_probability")?;
        if let Some(bytes) = net.corrupt_bytes {
            // Bounded: every armed datagram draws eight bytes per flip.
            if bytes == 0 || bytes > 64 {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: network.corrupt_bytes is {bytes}; an altered datagram has 1 to 64 \
                     bytes altered"
                )));
            }
            network.corrupt_bytes = bytes;
        }
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
            // Refused; strict behaviour under "lenient" would mislead.
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
        if let Some(reorder) = raw_disk.reorder_unsynced {
            disk.reorder_unsynced = reorder;
        }
        if let Some(depth) = raw_disk.snapshot_depth {
            disk.snapshot_depth = depth;
        }
    }

    let mut fault_policy = FaultPolicy::default();
    let mut fault_points = Vec::new();
    let mut fault_point_odds = (1, 2);
    if let Some(raw_faults) = &raw.faults {
        if let Some(names) = &raw_faults.fault_points {
            // The SDKs split the list on commas.
            if let Some(bad) = names.iter().find(|n| n.is_empty() || n.contains(',')) {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: faults.fault_points {bad:?} is not a name a fault point can have"
                )));
            }
            fault_points.clone_from(names);
        }
        if raw_faults.fault_point_probability.is_some() {
            fault_point_odds = probability(
                raw_faults.fault_point_probability.as_deref(),
                "faults.fault_point_probability",
            )?;
        }
        fault_policy.kill =
            probability(raw_faults.kill_probability.as_deref(), "faults.kill_probability")?;
        fault_policy.enomem =
            probability(raw_faults.enomem_probability.as_deref(), "faults.enomem_probability")?;
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
        fault_policy.disarm_after = raw_faults.disarm_after_ticks.map(bohr_types::Tick);
        for name in raw_faults.ablate.iter().flatten() {
            let kind = bohr_trace::FaultKind::named(name).ok_or_else(|| {
                ConfigError::Refused(format!(
                    "BOHR-1002: faults.ablate names {name:?}, which is no fault kind; the kinds are \
                     {}",
                    bohr_trace::FaultKind::ALL.map(bohr_trace::FaultKind::name).join(", ")
                ))
            })?;
            fault_policy.suppressed = fault_policy.suppressed.with(kind);
        }
        if let Some(closes) = fault_policy.disarm_after
            && closes <= fault_policy.arm_after
        {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: faults.disarm_after_ticks is {} and faults.arm_after_ticks is {}, so \
                 no drawn fault could ever fire",
                closes.get(),
                fault_policy.arm_after.get()
            )));
        }
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
            // Kinds must be named, so a campaign searches only what its scenario says.
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
                    // A recovery mode of the kill.
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
            // In `[[fault]]` with `at_tick`, so reduction can move it.
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
                // Refused at load, before the clock would panic.
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
        // A clock key on another kind is refused.
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
        // A cluster-wide fault names no node; a node fault must.
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
        // A node outside the cluster is refused (§8 rule 6); the world would create it.
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
            // Neither is required; without one the command line's program runs. Both together, or
            // fewer programs than nodes, are refused.
            if !c.argv.is_empty() && !c.programs.is_empty() {
                return Err(ConfigError::Refused(
                    "BOHR-1002: [cluster] takes argv or programs, not both".to_owned(),
                ));
            }
            // A cluster past this is no test a host can run, and each node costs memory up front.
            if c.nodes > MAX_NODES {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: [cluster] nodes = {} is past the {MAX_NODES} a world holds",
                    c.nodes
                )));
            }
            if !c.programs.is_empty() && c.programs.len() != c.nodes as usize {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: [cluster] names {} programs for {} nodes",
                    c.programs.len(),
                    c.nodes
                )));
            }
            if !c.roles.is_empty() && c.roles.len() != c.nodes as usize {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: [cluster] names {} roles for {} nodes",
                    c.roles.len(),
                    c.nodes
                )));
            }
            if let Some(unknown) = c.roles.iter().find(|r| !ROLES.contains(&r.as_str())) {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: [cluster] role {unknown:?} is none of {}",
                    ROLES.join(", ")
                )));
            }
            Some(Cluster {
                nodes: c.nodes,
                argv: c.argv,
                programs: c.programs,
                env: c.env.into_iter().collect(),
                roles: c.roles,
            })
        }
    };

    // The load is a node of the run too, numbered after the scenario's own: a run may end with it.
    let loading = u32::from(raw.workload.is_some());
    let run = run_limits(raw.run.as_ref(), cluster.as_ref().map(|c| c.nodes + loading))?;
    let (state_source, cwd) = state(raw.state.as_ref());
    let differential =
        differential(raw.differential.as_ref(), raw.faults.as_ref(), cluster.as_ref())?;
    let liveness = liveness(raw.liveness.as_ref(), &raw.partition)?;
    let durability = durability(raw.durability.as_ref(), raw.faults.as_ref(), &raw.fault)?;
    let probes = probes(raw.chain.as_ref(), &raw.probe, cluster.as_ref())?;
    let workload = crate::adapter::workload(
        raw.workload.as_ref(),
        raw.adapter.as_ref(),
        cluster.as_ref().map_or(1, |c| c.nodes.max(1)),
        fault_policy.disarm_after.map(Tick::get),
    )?;
    let model = model(
        raw.model.as_ref(),
        cluster.is_some(),
        workload.as_ref(),
        &disk,
        &node_faults,
        &fault_policy,
        &network,
    )?;

    // A policy armed after the run ends never fires, so it is refused with both numbers.
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
    // A window closing at or after the end closes nothing, which a scenario would not mean.
    if let (Some(horizon), Some(closes)) = (run.horizon, fault_policy.disarm_after)
        && closes >= horizon
    {
        return Err(ConfigError::Refused(format!(
            "BOHR-1002: faults.disarm_after_ticks is {} and run.horizon_ticks is {}, so the window \
             would close after the run ended",
            closes.get(),
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
        state: None,
        state_source,
        cwd,
        differential,
        coverage_bytes: coverage(raw.coverage.as_ref())?,
        linux: linux(raw.linux.as_ref(), &disk)?,
        probes,
        fault_points,
        fault_point_odds,
        model,
        workload,
    })
}

/// Reads `[model]`, refusing a family nothing could feed.
///
/// R1.4 is judged only where nothing the scenario arms is graded below `real`: a torn or reordered
/// crash, a restart from an older snapshot or a corrupted packet can each leave a node rightly
/// unable to start.
fn model(
    raw: Option<&RawModel>,
    clustered: bool,
    workload: Option<&crate::adapter::Workload>,
    disk: &crate::DiskConfig,
    node_faults: &[ScheduledNodeFault],
    fault_policy: &FaultPolicy,
    network: &NetworkConfig,
) -> Result<Option<crate::model::ModelConfig>, ConfigError> {
    let refused = |m: String| ConfigError::Refused(format!("BOHR-1002: {m}"));
    let loaded = workload.is_some();
    let Some(raw) = raw else {
        return if loaded {
            Err(refused(
                "[workload] feeds the chain model, and no [model] kind = \"chain\" reads it"
                    .to_owned(),
            ))
        } else {
            Ok(None)
        };
    };
    let family = match raw.kind.as_str() {
        "chain" => crate::model::Family::Chain,
        "kv" => crate::model::Family::Kv,
        "log" => crate::model::Family::Log,
        other => {
            return Err(refused(format!(
                "model.kind has no family named {other:?}; this build knows chain, kv and log"
            )));
        }
    };
    if !clustered {
        return Err(refused(
            "[model] reads the records a cluster's nodes write, and the scenario has no [cluster]"
                .to_owned(),
        ));
    }
    if (family == crate::model::Family::Chain) != loaded {
        return Err(refused(
            "a chain is judged from what the load records: model.kind = \"chain\" and [workload] \
             go together"
                .to_owned(),
        ));
    }
    if node_faults.iter().any(|f| matches!(f.kind, crate::NodeFaultKind::ChainReorg { .. })) {
        return Err(refused(
            "chain_reorg rewinds every node below what it committed, by design; the laws of \
             [model] are not judged under it"
                .to_owned(),
        ));
    }
    let snapshot = node_faults
        .iter()
        .any(|f| matches!(f.kind, crate::NodeFaultKind::RestartFromSnapshot { .. }));
    let restart_judged = !disk.reorder_unsynced
        && !snapshot
        && !fault_policy.kinds.recover_from_snapshot
        && network.corrupt_probability.0 == 0;
    Ok(Some(crate::model::ModelConfig {
        family,
        ready_within: Tick(raw.ready_within_ticks.unwrap_or(10_000_000_000)),
        restart_judged,
        moves_within: Tick(raw.moves_within_ticks.unwrap_or(10_000_000_000)),
        window_close: judged_close(
            window_close(node_faults, fault_policy, network, disk),
            raw.moves_within_ticks.unwrap_or(10_000_000_000),
            workload,
        ),
    }))
}

/// The window's close, or never where R3 would be judged after the load's last observation: a
/// chain's heads come from the load, and silence after it ends is not a stall.
fn judged_close(
    close: Tick,
    moves_within: u64,
    workload: Option<&crate::adapter::Workload>,
) -> Tick {
    match workload {
        Some(w) if close.get().saturating_add(moves_within) > w.window_ticks + w.settle_ticks => {
            Tick(u64::MAX)
        }
        _ => close,
    }
}

/// When the last fault a scenario arms can land: the latest of its scheduled node faults, its
/// partitions' and grey windows' ends, and, for drawn faults, the moment no more fire plus the time
/// a killed node takes to come back. Drawn faults with no closing moment never close it.
fn window_close(
    node_faults: &[ScheduledNodeFault],
    fault_policy: &FaultPolicy,
    network: &NetworkConfig,
    disk: &crate::DiskConfig,
) -> Tick {
    let drawn = fault_policy.kill.0 > 0
        || fault_policy.enomem.0 > 0
        || network.drop_probability.0 > 0
        || network.duplicate_probability.0 > 0
        || network.reorder_probability.0 > 0
        || network.corrupt_probability.0 > 0
        || disk.stall_probability.0 > 0
        || disk.eio_probability.0 > 0
        || disk.enospc_probability.0 > 0;
    // A drawn node fault with no recovery never heals: the window it opens does not close.
    if fault_policy.kill.0 > 0 && fault_policy.restart_after.get() == 0 {
        return Tick(u64::MAX);
    }
    let mut close = match (drawn, fault_policy.disarm_after) {
        (true, None) => return Tick(u64::MAX),
        (true, Some(closes)) => closes.get().saturating_add(fault_policy.restart_after.get()),
        (false, _) => 0,
    };
    for fault in node_faults {
        close = close.max(fault.at_tick.get());
    }
    for cut in &network.partitions {
        close = close.max(cut.heal_at_tick.get());
    }
    for window in &network.grey {
        close = close.max(window.until_tick.get());
    }
    Tick(close)
}

/// The port JSON-RPC is answered on where a probe names none: Ethereum's.
const DEFAULT_RPC_PORT: u16 = 8545;

/// Reads `[chain]` and every `[[probe]]` into the probes they amount to.
///
/// Each oracle is a predefined probe: `finality` is `never-reverts` on the finalised hash by
/// number, `agreement` is `agrees` on the same, `liveness` is `advances` on the number, and
/// `double-sign` is `unique` on votes by signer and height. Unknown nodes, probes without a cluster
/// and stray spans are refused.
fn probes(
    chain: Option<&RawChain>,
    raw: &[RawProbe],
    cluster: Option<&Cluster>,
) -> Result<Vec<ProbeSpec>, ConfigError> {
    if chain.is_none() && raw.is_empty() {
        return Ok(Vec::new());
    }
    let Some(cluster) = cluster else {
        return Err(ConfigError::Refused(
            "BOHR-1002: a probe asks the nodes of a [cluster], and this scenario has none"
                .to_owned(),
        ));
    };
    let all: Vec<u32> = (0..cluster.nodes).collect();
    let known = |nodes: &[u32], what: &str| -> Result<(), ConfigError> {
        match nodes.iter().find(|n| **n >= cluster.nodes) {
            Some(missing) => Err(ConfigError::Refused(format!(
                "BOHR-1002: {what} names node {missing}, and this cluster has {}",
                cluster.nodes
            ))),
            None if nodes.is_empty() => {
                Err(ConfigError::Refused(format!("BOHR-1002: {what} asks no node")))
            }
            None => Ok(()),
        }
    };
    let mut out = Vec::new();

    if let Some(chain) = chain {
        let nodes = chain.nodes.clone().unwrap_or_else(|| all.clone());
        known(&nodes, "[chain]")?;
        if chain.every_ticks == 0 {
            return Err(ConfigError::Refused(
                "BOHR-1002: chain.every_ticks is zero, which asks without pause".to_owned(),
            ));
        }
        if chain.oracles.is_empty() {
            return Err(ConfigError::Refused(
                "BOHR-1002: [chain] names no oracle; this build has finality, agreement, liveness \
                 and double-sign"
                    .to_owned(),
            ));
        }
        let port = chain.port.unwrap_or(DEFAULT_RPC_PORT);
        let finalized = |name: &str, key: &[&str], value: &str, property, within| ProbeSpec {
            name: name.to_owned(),
            nodes: nodes.clone(),
            port,
            rpc: "eth_getBlockByNumber".to_owned(),
            params: "[\"finalized\",false]".to_owned(),
            every: Tick(chain.every_ticks),
            key: key.iter().map(|k| (*k).to_owned()).collect(),
            value: value.to_owned(),
            property,
            within,
        };
        for oracle in &chain.oracles {
            let spec = match oracle.as_str() {
                "finality" => {
                    finalized("finality", &["number"], "hash", Property::NeverReverts, None)
                }
                "agreement" => finalized("agreement", &["number"], "hash", Property::Agrees, None),
                "liveness" => {
                    let Some(stall) = chain.stall_ticks.filter(|s| *s > 0) else {
                        return Err(ConfigError::Refused(
                            "BOHR-1002: the liveness oracle needs chain.stall_ticks, how long a \
                             finalised height may stay put once the cluster is whole"
                                .to_owned(),
                        ));
                    };
                    finalized("liveness", &[], "number", Property::Advances, Some(Tick(stall)))
                }
                "double-sign" => {
                    let Some(rpc) = chain.votes_rpc.clone() else {
                        return Err(ConfigError::Refused(
                            "BOHR-1002: the double-sign oracle needs chain.votes_rpc, the method \
                             that lists the votes a node signed or saw as signer, height and hash"
                                .to_owned(),
                        ));
                    };
                    ProbeSpec {
                        name: "double-sign".to_owned(),
                        nodes: nodes.clone(),
                        port,
                        rpc,
                        params: "[]".to_owned(),
                        every: Tick(chain.every_ticks),
                        key: vec!["signer".to_owned(), "height".to_owned()],
                        value: "hash".to_owned(),
                        property: Property::Unique,
                        within: None,
                    }
                }
                other => {
                    return Err(ConfigError::Refused(format!(
                        "BOHR-1002: [chain] has no oracle named {other:?}; this build has \
                         finality, agreement, liveness and double-sign"
                    )));
                }
            };
            if out.iter().any(|s: &ProbeSpec| s.name == spec.name) {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: [chain] names {oracle:?} twice"
                )));
            }
            out.push(spec);
        }
        if chain.stall_ticks.is_some() && !chain.oracles.iter().any(|o| o == "liveness") {
            return Err(ConfigError::Refused(
                "BOHR-1002: chain.stall_ticks is read by the liveness oracle, and [chain] does not \
                 name it"
                    .to_owned(),
            ));
        }
        if chain.votes_rpc.is_some() && !chain.oracles.iter().any(|o| o == "double-sign") {
            return Err(ConfigError::Refused(
                "BOHR-1002: chain.votes_rpc is read by the double-sign oracle, and [chain] does \
                 not name it"
                    .to_owned(),
            ));
        }
    }

    for (i, probe) in raw.iter().enumerate() {
        let what = format!("[[probe]] {}", i + 1);
        let nodes = match (probe.node, &probe.nodes) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: {what} takes node or nodes, not both"
                )));
            }
            (Some(node), None) => vec![node],
            (None, Some(nodes)) => nodes.clone(),
            (None, None) => all.clone(),
        };
        known(&nodes, &what)?;
        let Some(property) = Property::parse(&probe.property) else {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: {what} has no property named {:?}; this build has never-reverts, \
                 monotonic, agrees, advances and unique",
                probe.property
            )));
        };
        if probe.every_ticks == 0 {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: {what}: every_ticks is zero, which asks without pause"
            )));
        }
        let within = match (property, probe.within_ticks) {
            (Property::Advances, Some(w)) if w > 0 => Some(Tick(w)),
            (Property::Advances, _) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: {what}: advances needs within_ticks above zero, the span a value \
                     may stay the same"
                )));
            }
            (_, Some(_)) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: {what}: within_ticks is read by advances only"
                )));
            }
            (_, None) => None,
        };
        let key = match &probe.key {
            None => Vec::new(),
            Some(toml::Value::String(one)) => vec![one.clone()],
            Some(toml::Value::Array(many)) => many
                .iter()
                .map(|k| k.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    ConfigError::Refused(format!("BOHR-1002: {what}: key names fields as strings"))
                })?,
            Some(_) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: {what}: key is a field name or a list of them"
                )));
            }
        };
        if property == Property::Advances && !key.is_empty() {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: {what}: advances watches one value and reads no key"
            )));
        }
        if probe.value.is_empty() || key.iter().any(String::is_empty) || probe.rpc.is_empty() {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: {what}: rpc, value and every key field are named, never empty"
            )));
        }
        // An array is positional parameters, a table named ones; CometBFT takes only named.
        let params = match &probe.params {
            None => bohr_sdk_proto::json::Json::Array(Vec::new()),
            Some(value @ (toml::Value::Array(_) | toml::Value::Table(_))) => json_of(value)
                .ok_or_else(|| {
                    ConfigError::Refused(format!(
                        "BOHR-1002: {what}: params hold strings, integers, booleans, arrays and \
                         tables; a float or a date has no single JSON spelling"
                    ))
                })?,
            Some(_) => {
                return Err(ConfigError::Refused(format!(
                    "BOHR-1002: {what}: params is an array of positional parameters or a table \
                     of named ones"
                )));
            }
        };
        let name = probe.name.clone().unwrap_or_else(|| format!("{}#{}", probe.rpc, i + 1));
        if out.iter().any(|s: &ProbeSpec| s.name == name) {
            return Err(ConfigError::Refused(format!(
                "BOHR-1002: {what}: a probe is already named {name:?}"
            )));
        }
        out.push(ProbeSpec {
            name,
            nodes,
            port: probe.port.unwrap_or(DEFAULT_RPC_PORT),
            rpc: probe.rpc.clone(),
            params: params.to_json(),
            every: Tick(probe.every_ticks),
            key,
            value: probe.value.clone(),
            property,
            within,
        });
    }
    Ok(out)
}

/// A TOML value as JSON, where it has exactly one JSON spelling.
fn json_of(value: &toml::Value) -> Option<bohr_sdk_proto::json::Json> {
    use bohr_sdk_proto::json::Json;
    Some(match value {
        toml::Value::String(s) => Json::Str(s.clone()),
        toml::Value::Integer(i) => Json::Int(*i),
        toml::Value::Boolean(b) => Json::Bool(*b),
        toml::Value::Array(items) => Json::Array(items.iter().map(json_of).collect::<Option<_>>()?),
        toml::Value::Table(table) => Json::Object(
            table.iter().map(|(k, v)| Some((k.clone(), json_of(v)?))).collect::<Option<_>>()?,
        ),
        toml::Value::Float(_) | toml::Value::Datetime(_) => return None,
    })
}

/// Reads `[linux]`, refusing a ring the disk model could not keep its promises through.
///
/// A stall cannot happen inside one `io_uring_enter`, so a ring with drawn disk stalls is refused
/// at load.
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
/// The map is copied once per run and held per worker, so its size is a continuous cost.
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

    // Refused: a stalled cluster still gossips, so packets are not progress.
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
    // A heal-armed watch with nothing ever cut never fires and is refused; the stopped-node watch
    // needs no cut.
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
/// `file:1:/head` is node one's; `file:/head` is node zero's. Paths are absolute, so a prefix
/// before the colon is a node number.
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
    // Drawn faults draw per syscall, which differs between implementations; scheduled faults line
    // up. See `docs/ARCHITECTURE.md` §5.20.
    if faults.and_then(|f| f.kill_probability.as_deref()).is_some() {
        return Err(ConfigError::Refused(
            "BOHR-1002: faults.kill_probability draws per syscall, and two implementations make \
             different syscalls, so a drawn fault schedule cannot be the same for both; use a \
             [[fault]] at a tick, which is"
                .to_owned(),
        ));
    }

    // Every implementation must name the same node, inside the cluster.
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

/// Where a prepared state is on the host, and the digest it is pinned to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StateSource {
    /// The directory, as the scenario names it.
    pub dir: String,
    /// The digest the scenario pins it to, where it pins one.
    pub digest: Option<String>,
}

/// Reads `[state]`: where the state is and what it is pinned to, and the working directory.
fn state(raw: Option<&RawState>) -> (Option<StateSource>, Option<String>) {
    let Some(raw) = raw else {
        return (None, None);
    };
    (Some(StateSource { dir: raw.dir.clone(), digest: raw.digest.clone() }), raw.cwd.clone())
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

    // An answer is approximated as reaching outward with unsynced writes.
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
    // Refused without kills, since the finding needs a crash.
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
    // A horizon past the budget never fires; refused with both numbers.
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

    // A node outside the cluster never ends, so it is refused.
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
        // The refusal includes the nearest expressible value.
        return Err(ConfigError::Refused(format!(
            "BOHR-1003: {key} denominator {den} is outside 1..={MAX_DENOMINATOR}; \
             nearest expressible: {num}/{MAX_DENOMINATOR}"
        )));
    }
    // A probability above one is refused.
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

    /// A cluster past the bound is refused before a node is made.
    #[test]
    fn a_cluster_past_the_bound_is_refused() {
        assert!(parse("[cluster]\nnodes = 1024\n").is_ok());
        let refused = parse("[cluster]\nnodes = 4294967295\n");
        assert!(
            matches!(&refused, Err(ConfigError::Refused(why)) if why.contains("nodes")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_full_scenario_parses_into_the_world_types() {
        let loaded = parse(
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
        let loaded = parse(
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
    /// Both bounds refuse, never clamp.
    #[test]
    fn a_coverage_map_too_small_to_read_or_too_large_to_hold_is_refused() {
        let sized = |bytes: &str| {
            parse(&format!(
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
            parse(
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
        let err = parse("[network]\ndrop_probability = \"1/100000\"\n").expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("BOHR-1003") && text.contains("1/65536"), "got: {text}");
    }

    #[test]
    fn a_fault_window_is_read_and_an_empty_or_late_one_is_refused() {
        let faults = "[faults]\nkill_probability = \"1/1000\"\narm_after_ticks = 5\n";
        let loaded = parse(&format!("{faults}disarm_after_ticks = 9\n")).expect("a window");
        assert_eq!(loaded.fault_policy.disarm_after, Some(bohr_types::Tick(9)));

        let empty = parse(&format!("{faults}disarm_after_ticks = 5\n")).expect_err("refused");
        assert!(empty.to_string().contains("faults.disarm_after_ticks is 5"), "got: {empty}");

        let late = parse(&format!("[run]\nhorizon_ticks = 9\n{faults}disarm_after_ticks = 9\n"))
            .expect_err("refused");
        assert!(late.to_string().contains("would close after the run ended"), "got: {late}");
    }

    #[test]
    fn an_ablation_names_kinds_and_refuses_a_name_that_is_none() {
        let loaded = parse("[faults]\nkill_probability = \"1/1000\"\nablate = [\"disk_tear\"]\n")
            .expect("an ablation");
        assert!(loaded.fault_policy.suppressed.contains(bohr_trace::FaultKind::DiskTear));
        assert!(!loaded.fault_policy.suppressed.contains(bohr_trace::FaultKind::NodeKill));
        let err = parse("[faults]\nablate = [\"disk_torn\"]\n").expect_err("refused");
        assert!(err.to_string().contains("\"disk_torn\""), "got: {err}");
    }

    #[test]
    fn an_unknown_key_is_refused_not_skipped() {
        assert!(parse("[network]\nlatencccy = 5\n").is_err(), "silent skipping hides typos");
    }

    #[test]
    fn lenient_disk_semantics_are_refused_until_they_exist() {
        let err = parse("[disk]\nsync_semantics = \"lenient\"\n").expect_err("refused");
        assert!(err.to_string().contains("strict"));
    }

    /// The ring is asked for by name, absent by default, and a third value is a typo.
    #[test]
    fn a_ring_is_named_or_absent() {
        assert_eq!(parse("").unwrap().linux.io_uring, IoUring::Absent);
        assert_eq!(
            parse("[linux]\nio_uring = \"absent\"\n").unwrap().linux.io_uring,
            IoUring::Absent
        );
        assert_eq!(parse("[linux]\nio_uring = \"ring\"\n").unwrap().linux.io_uring, IoUring::Ring);
        let err = parse("[linux]\nio_uring = \"sqpoll\"\n").expect_err("refused");
        assert!(err.to_string().contains("BOHR-1002"), "{err}");
    }

    /// A ring with drawn disk stalls is refused.
    #[test]
    fn a_ring_with_stalling_syncs_is_refused() {
        let err = parse("[linux]\nio_uring = \"ring\"\n[disk]\nstall_probability = \"1/10\"\n")
            .expect_err("refused");
        assert!(err.to_string().contains("stall"), "{err}");
        assert!(parse("[disk]\nstall_probability = \"1/10\"\n").is_ok(), "without a ring it runs");
    }

    /// An implementation names the node it replaces.
    #[test]
    fn an_implementation_stands_in_for_a_node_the_cluster_has() {
        let compared = |first: &str, second: &str| {
            parse(&format!(
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

    /// A source names its node; without one it is node zero.
    #[test]
    fn a_projection_source_carries_the_node_that_holds_it() {
        let read = |source: &str| {
            parse(&format!(
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

    /// Each chain oracle is a predefined probe asking every node unless narrowed.
    #[test]
    fn the_chain_oracles_are_probes_of_every_node() {
        let loaded = parse(
            r#"
            [cluster]
            nodes = 3
            argv = ["./node"]

            [chain]
            oracles = ["finality", "agreement", "liveness", "double-sign"]
            every_ticks = 100
            stall_ticks = 1000
            votes_rpc = "chain_votes"
            "#,
        )
        .expect("loads");
        let names: Vec<&str> = loaded.probes.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["finality", "agreement", "liveness", "double-sign"]);
        assert!(loaded.probes.iter().all(|p| p.nodes == [0, 1, 2] && p.port == 8545));
        assert_eq!(loaded.probes[2].within, Some(Tick(1000)));
        assert_eq!(loaded.probes[3].key, ["signer", "height"]);
        let watching = loaded.watching();
        assert!(watching.probed.never_reverts && watching.probed.agrees);
        assert!(watching.probed.advances && watching.probed.unique && !watching.probed.monotonic);
    }

    /// A probe written by hand names its call, its value and its property, and only what it asks.
    #[test]
    fn a_probe_reads_its_params_as_json() {
        let loaded = parse(
            r#"
            [cluster]
            nodes = 2
            argv = ["./node"]

            [[probe]]
            node = 1
            port = 9000
            rpc = "eth_getBlockByNumber"
            params = ["latest", false, { full = true }]
            every_ticks = 50
            key = "number"
            value = "hash"
            property = "never-reverts"
            "#,
        )
        .expect("loads");
        let probe = &loaded.probes[0];
        assert_eq!(probe.nodes, [1]);
        assert_eq!(probe.params, r#"["latest",false,{"full":true}]"#);
        assert_eq!(probe.name, "eth_getBlockByNumber#1");
    }

    /// Every probe mistake is refused where it is read, by name.
    #[test]
    fn a_probe_that_could_not_mean_anything_is_refused() {
        let refused = |text: &str, says: &str| {
            let err = parse(text).expect_err("refused");
            assert!(err.to_string().contains(says), "{says:?} not in: {err}");
        };
        let cluster = "[cluster]\nnodes = 2\nargv = [\"./node\"]\n";
        refused("[chain]\noracles = [\"finality\"]\nevery_ticks = 10\n", "has none");
        refused(
            &format!("{cluster}[chain]\noracles = [\"liveness\"]\nevery_ticks = 10\n"),
            "stall_ticks",
        );
        refused(
            &format!("{cluster}[chain]\noracles = [\"double-sign\"]\nevery_ticks = 10\n"),
            "votes_rpc",
        );
        refused(
            &format!(
                "{cluster}[chain]\noracles = [\"finality\"]\nevery_ticks = 10\nstall_ticks = 5\n"
            ),
            "does not name it",
        );
        refused(
            &format!("{cluster}[chain]\noracles = [\"safety\"]\nevery_ticks = 10\n"),
            "no oracle named",
        );
        refused(
            &format!("{cluster}[chain]\noracles = [\"finality\"]\nnodes = [2]\nevery_ticks = 10\n"),
            "names node 2",
        );
        refused(
            &format!("{cluster}[chain]\noracles = [\"finality\"]\nevery_ticks = 0\n"),
            "without pause",
        );
        let probe = |extra: &str| {
            format!("{cluster}[[probe]]\nrpc = \"m\"\nevery_ticks = 10\nvalue = \"v\"\n{extra}")
        };
        refused(&probe("property = \"sometimes\"\n"), "no property named");
        refused(&probe("property = \"advances\"\n"), "within_ticks above zero");
        refused(&probe("property = \"agrees\"\nwithin_ticks = 5\n"), "read by advances only");
        refused(&probe("property = \"agrees\"\nparams = [1.5]\n"), "a float");
        refused(&probe("property = \"agrees\"\nnode = 0\nnodes = [1]\n"), "not both");
    }
}
